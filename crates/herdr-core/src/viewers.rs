//! Viewers: raw attach streams on framed connections. First paint, live
//! bytes (minus answered queries), input, and who sets the size.

use crate::holder_proto as proto;
use crate::relay::Relay;
use crate::rpc::RpcError;
use crate::runtime::{Core, State};
use crate::wire::Conn;
use ghostty_vt::Rgb;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(test)]
mod tests;

/// What a viewer's own terminal looks like, reported by the client.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default, Debug, PartialEq)]
pub struct Display {
    /// Default foreground as `#rrggbb`.
    pub fg: Option<String>,
    /// Default background as `#rrggbb`.
    pub bg: Option<String>,
    /// Cursor color as `#rrggbb`.
    pub cursor: Option<String>,
    /// Cell size in pixels: `[width, height]`.
    pub cell: Option<[u32; 2]>,
}

pub struct Viewer {
    pub conn: Arc<Conn>,
    pub channel: u32,
    pub observe: bool,
    resize_notifications: bool,
    /// Scrollback rows each repaint after a RESIZE carries; 0: screen only.
    resize_history: usize,
    repaint_pending: bool,
    size: (u16, u16),
    /// None: not active since it came back after a dropped line.
    active_at: Option<Instant>,
    /// Desktop apps attach hidden tabs with a placeholder before reporting geometry.
    sized: bool,
    /// The effective compositor visibility. Old clients default to true.
    visible: bool,
    /// A relayed reconnect carries cached geometry, not a new physical offer.
    awaiting_size: bool,
    /// The attach path negotiated the viewer-to-core VISIBILITY frame.
    visibility_updates: bool,
    /// The attach path negotiated WINDOW_FOCUS: this viewer reports keyboard focus.
    focus_updates: bool,
    /// Its window has keyboard focus (only meaningful with `focus_updates`).
    focused: bool,
    /// When its window last gained focus; None: never focused while attached.
    focused_at: Option<Instant>,
    /// Its last real input (DATA a person typed, clicked or pasted; never a
    /// terminal reply, AUTOMATIC_INPUT, output, CREDIT or a size offer).
    /// With `focused_at`, when a focus reporter was last used.
    input_at: Option<Instant>,
    /// A terminal reply cut across DATA frames is still a reply.
    replies: crate::input::Replies,
    /// Origin is captured at attach so peer-line cleanup cannot relabel a viewer.
    origin: ViewerOrigin,
    peer: Option<String>,
    /// The source line's machine and the desktop identity carried on that line.
    desktop_group: Option<(String, String)>,
    display: Display,
    /// An unfinished sequence exceeded continuation tracking; skip its tail.
    pub(crate) recovering: bool,
}

fn viewer_origin(st: &State, conn: &Arc<Conn>) -> (ViewerOrigin, Option<String>) {
    let key = crate::remote::conn_key(conn);
    let peer_id = st.peer_lines.iter().find(|(_, (conn, _))| *conn == key).map(|(id, _)| id);
    if let Some(id) = peer_id {
        let peer = st.peers.get(id).map(|peer| peer.name.clone()).unwrap_or_else(|| id.clone());
        (ViewerOrigin::Relayed, Some(peer))
    } else if conn.local_desktop {
        (ViewerOrigin::LocalDesktop, None)
    } else {
        (ViewerOrigin::LocalClient, None)
    }
}

#[derive(Serialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewerIdentity {
    /// Monotonic connection ID, unique for this core process lifetime.
    pub connection_id: u64,
    pub channel: u32,
}

#[derive(Serialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Serialize, JsonSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ViewerOrigin {
    LocalDesktop,
    LocalClient,
    Relayed,
}

#[derive(Serialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct ViewerDesktopGroup {
    pub line: String,
    pub group: String,
}

#[derive(Serialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct ViewerInfo {
    pub identity: ViewerIdentity,
    pub origin: ViewerOrigin,
    /// Connected peer name for a relayed viewer; absent for local connections.
    pub peer: Option<String>,
    /// Same-uid local Unix socket peer PID (possibly an `hc connect` child, not its GPUI parent).
    /// Never populated for relayed connections; unavailable credentials/PID are null.
    pub peer_pid: Option<u32>,
    pub offered: GridSize,
    /// Effective visibility; legacy clients remain visible until detach.
    pub visible: bool,
    /// Candidate in the current sizing selector (can be false while visible).
    pub size_eligible: bool,
    pub visibility_updates: bool,
    /// Negotiated WINDOW_FOCUS: the viewer reports whether its window has keyboard focus.
    pub focus_updates: bool,
    /// Its window has keyboard focus now (false without `focus_updates`).
    pub focused: bool,
    /// Milliseconds since its window last gained keyboard focus; null if never.
    pub focused_ms_ago: Option<u64>,
    pub resize_notifications: bool,
    pub observe: bool,
    pub sized: bool,
    pub awaiting_size: bool,
    pub desktop_group: Option<ViewerDesktopGroup>,
    /// Milliseconds since last focus/input/size/visibility activity.
    pub active_ms_ago: Option<u64>,
    /// Milliseconds since its last real input (typed, clicked, pasted DATA;
    /// never terminal replies or AUTOMATIC_INPUT). With `focused_ms_ago`,
    /// when a focus reporter was last used; the latest used decider leads.
    pub input_ms_ago: Option<u64>,
    pub leader: bool,
    /// Included in the selected group's min-grid, or the selected fallback size.
    pub contributes_to_grid: bool,
}

#[derive(Serialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct ViewerListResult {
    pub terminal: String,
    pub current_grid: GridSize,
    /// What the same selector used by the live resizer currently chooses.
    pub selected_grid: Option<GridSize>,
    /// A focus-reporting viewer has been focused: only the focused (or last
    /// focused) window may set the grid; everyone else only receives it.
    pub focus_authority: bool,
    pub leader: Option<ViewerIdentity>,
    pub viewers: Vec<ViewerInfo>,
    /// A person's unsent draft, which agents' text waits for; absent: none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<crate::input::DraftView>,
}

/// Terminal `id`'s unsent draft, after ending it if it expired (see
/// `Gate::expire`; the core's log says so). Absent: agents' text goes in.
pub(crate) fn draft(st: &mut State, id: u32) -> Option<crate::input::DraftView> {
    let viewers = st.viewers.get(&id).map_or(&[][..], Vec::as_slice);
    let interactive = viewers.iter().any(|v| !v.observe);
    let attached: Vec<(u64, u32)> = viewers.iter().map(|v| (v.conn.id, v.channel)).collect();
    let gate = st.gates.get_mut(&id)?;
    let before = gate.view(|by| attached.contains(&(by.connection_id, by.channel)), interactive)?;
    if let Some(why) = gate.expire(interactive) {
        let by =
            before.by.as_ref().map(|b| format!(" by viewer c{}/ch{}", b.connection_id, b.channel)).unwrap_or_default();
        crate::log!(
            "core: {}: the draft a person began{by} {} s ago expired ({why}); agents' text types again",
            crate::terminals::address(id),
            crate::runtime::now_ms().saturating_sub(before.since_ms) / 1000,
        );
        return None;
    }
    Some(before)
}

impl Display {
    /// Answer the program's color and cell-size queries with these facts.
    pub(crate) fn apply(&self, screen: &mut ghostty_vt::Terminal) {
        screen.set_display(rgb(&self.fg), rgb(&self.bg), rgb(&self.cursor), self.cell.map(|[w, h]| (w, h)));
    }
}

fn rgb(hex: &Option<String>) -> Option<Rgb> {
    let hex = hex.as_deref()?.strip_prefix('#')?;
    let n = u32::from_str_radix(hex, 16).ok().filter(|_| hex.len() == 6)?;
    Some([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

pub struct AttachRequest {
    pub observe: bool,
    pub resize_notifications: bool,
    pub history: usize,
    /// Scrollback rows each repaint after a RESIZE carries; 0: screen only.
    pub resize_history: usize,
    pub size: (u16, u16),
    pub display: Display,
    pub resumed: bool,
    pub visible: bool,
    pub visibility_updates: bool,
    pub focus_updates: bool,
    pub focused: bool,
    /// With `focus_updates`: how long ago the view was last used (an
    /// automatic reattach or reconnect, focused or not); None: a focused
    /// attach is the use, an unfocused one was not used.
    pub idle_ms: Option<u64>,
    pub desktop_group: Option<(String, String)>,
}

impl Core {
    /// Local desktop identity is observed; forwarded identity is accepted only
    /// from an established core line and namespaced by that line's machine.
    pub(crate) fn desktop_group(&self, conn: &Arc<Conn>, reported: Option<&str>) -> Option<(String, String)> {
        if conn.local_desktop {
            return Some((self.info.id.clone(), self.info.id.clone()));
        }
        let group = reported.filter(|group| !group.is_empty())?;
        let key = crate::remote::conn_key(conn);
        self.lock().peer_lines.iter().find(|(_, (k, _))| *k == key).map(|(id, _)| (id.clone(), group.to_owned()))
    }

    /// Adds a viewer: queues the first paint, then every live byte follows in order.
    pub fn attach(&self, conn: &Arc<Conn>, id: u32, req: AttachRequest) -> Result<u32, RpcError> {
        self.attach_record(conn, id, req).map(|(channel, _)| channel)
    }

    /// [`Core::attach`], and the terminal's record as it was in the same
    /// critical section that queued the first paint: its `incarnation` and
    /// `ctx` are those of the program the paint shows.
    pub fn attach_record(
        &self,
        conn: &Arc<Conn>,
        id: u32,
        req: AttachRequest,
    ) -> Result<(u32, crate::terminals::Terminal), RpcError> {
        let mut st = self.lock();
        if let Some(t) = st.reg.terminals.get(&id) {
            t.check_program()?;
        }
        if !st.screens.contains_key(&id) {
            return Err(RpcError::new(
                "finished",
                format!("{} has finished", crate::terminals::address(id)),
                format!("read what it left: hc read {}", crate::terminals::address(id)),
            ));
        }
        let channel = conn.new_channel();
        let (origin, peer) = viewer_origin(&st, conn);
        let recovering = st.screens[&id].continuation().is_none();
        st.viewers.entry(id).or_default().push(Viewer {
            conn: conn.clone(),
            channel,
            observe: req.observe,
            resize_notifications: req.resize_notifications,
            // Only RESIZE-confirmed viewers get resize paints with history.
            resize_history: if req.resize_notifications { req.resize_history.min(MAX_RESIZE_HISTORY) } else { 0 },
            repaint_pending: false,
            size: req.size,
            active_at: (!req.resumed).then(Instant::now),
            sized: false,
            visible: req.visible,
            awaiting_size: req.resumed,
            visibility_updates: req.visibility_updates,
            focus_updates: req.focus_updates,
            focused: req.focus_updates && req.focused,
            // A reattach of a viewer used before but not focused now still
            // says when it was last used (`idle_ms`): its history stays.
            focused_at: if !req.focus_updates {
                None
            } else if req.focused || req.idle_ms.is_some() {
                used_ago(req.idle_ms)
            } else {
                None
            },
            // A viewer that sends no focus, back after a dropped line (a
            // relay's reattach): its input recency comes along.
            input_at: if req.focus_updates { None } else { req.idle_ms.and_then(|ms| used_ago(Some(ms))) },
            replies: if req.resumed { crate::input::Replies::resumed() } else { Default::default() },
            origin,
            peer,
            desktop_group: req.desktop_group,
            display: req.display,
            recovering,
        });
        self.apply_size(&mut st, id);
        // Back after a dropped line with `resize_history`: a history
        // replacement may have been cut there after its ESC[3J, and nothing
        // here knows how far it got. The drop is the loss: the stream owes a
        // complete replacement until one is out whole (through an alternate
        // screen, or a repair dropped before it went out). Otherwise the
        // first paint as before.
        let resize_history = st.viewers[&id].last().unwrap().resize_history;
        let lost = req.resumed && resize_history > 0;
        let rows = viewer_rows(st.viewers[&id].last().unwrap(), &st.screens[&id]);
        let (paint, clears) = if lost {
            resize_paint_to(&st, id, resize_history, true, rows)
        } else {
            (first_paint_for(&st, id, req.history, rows), false)
        };
        st.viewers.get_mut(&id).unwrap().last_mut().unwrap().repaint_pending = false;
        conn.with(|out| {
            out.open_paused(channel);
            if req.resize_notifications {
                let t = &st.reg.terminals[&id];
                let size = [t.cols.to_le_bytes(), t.rows.to_le_bytes()].concat();
                out.resize(channel, &size);
            }
            // Whole, never refused (a large `history` must not vanish).
            out.paint(channel, &paint, clears);
            if lost {
                out.mark_history_lost(channel);
            }
        });
        Ok((channel, st.reg.terminals[&id].clone()))
    }

    /// Program output: through the relay to the screen and every viewer,
    /// then whatever status or title it reported.
    pub fn output(&self, st: &mut State, id: u32, bytes: &[u8]) {
        st.last_output.insert(id, Instant::now());
        *st.output_seq.entry(id).or_default() += 1;
        if let Some(t) = st.reg.terminals.get_mut(&id) {
            t.printed += bytes.iter().filter(|b| **b == b'\n').count() as u64;
        }
        // While a read scrolls the app, its frames are ours alone: viewers get one repaint after.
        let scrolling = st.stitching.contains(&id);
        let instance = st.reg.terminals.get(&id).and_then(|t| t.holder.clone());
        let State { screens, relays, viewers, holders, output_watch, .. } = st;
        let holder = holders.iter().find(|l| Some(&l.instance) == instance.as_ref());
        let Some(screen) = screens.get_mut(&id) else { return };
        let relay = relays.entry(id).or_default();
        let recovering = viewers.get(&id).into_iter().flatten().any(|v| v.recovering);
        let (out, recovered) =
            if recovering { relay.feed_to_ground(screen, bytes) } else { (relay.feed(screen, bytes), None) };
        let bounded_out = relay.bounded_output(&out, 0);
        let bounded_tail = recovered.as_ref().and_then(|(at, _)| relay.bounded_output(&out, *at));
        let rows = screen.size().1;
        let win32_input = relay.win32_input.take();
        if let Some(watchers) = output_watch.get_mut(&id).filter(|_| !scrolling) {
            watchers.retain(|w| w.send(out.clone()).is_ok());
        }
        if let Some(link) = holder {
            link.input(id, &Relay::take_responses(screen));
        }
        let reports = screen.take_status_reports();
        let title = screen.take_title_changed().then(|| screen.title());
        let full_screen = screen.alt_screen();
        // Before any repaint below, so it carries the mode this output just set.
        if let Some(on) = win32_input
            && let Some(t) = st.reg.terminals.get_mut(&id).filter(|t| t.win32_input != on)
        {
            t.win32_input = on;
            self.save(st);
        }
        let recovered = recovered.map(|(at, paint)| {
            let mut paint = paint_state(st, id, paint);
            let mut bounded = paint.clone();
            bounded.extend_from_slice(bounded_tail.as_deref().unwrap_or(&out[at..]));
            paint.extend_from_slice(&out[at..]);
            (paint, bounded)
        });
        let mut behind = Vec::new();
        let main_screen = !full_screen;
        for (index, v) in st.viewers.get_mut(&id).into_iter().flatten().enumerate().filter(|_| !scrolling) {
            if v.repaint_pending {
                continue;
            }
            // Owed a history repair (it was deferred while the program was on
            // the alternate screen): back on the main screen, repaint instead.
            if v.resize_history > 0 && main_screen && v.conn.outbox().repair_owed(v.channel) {
                v.repaint_pending = true;
                v.conn.wake.notify_one();
                continue;
            }
            let taller =
                !v.resize_notifications && !v.observe && v.sized && v.desktop_group.is_some() && v.size.1 > rows;
            let bytes = if v.recovering {
                let Some(paint) = &recovered else { continue };
                v.recovering = false;
                if taller { &paint.1 } else { &paint.0 }
            } else if taller {
                bounded_out.as_deref().unwrap_or(&out)
            } else {
                &out
            };
            v.conn.with(|ob| {
                if !ob.data(v.channel, bytes) {
                    behind.push(index);
                }
            });
        }
        // A viewer with `resize_history` may have been cut off in the middle
        // of a history replacement (after its ESC[3J): it gets a complete one
        // again, serialized when it has credit, like a resize paint.
        if let Some(viewers) = st.viewers.get_mut(&id) {
            behind.retain(|index| {
                let v = &mut viewers[*index];
                if v.resize_history > 0 {
                    v.repaint_pending = true;
                    v.conn.wake.notify_one();
                }
                v.resize_history == 0
            });
        }
        if !behind.is_empty() {
            let paint = first_paint(st, id, 0);
            let recovering = st.screens[&id].continuation().is_none();
            let viewers = st.viewers.get_mut(&id).expect("viewers with queued output");
            for index in behind {
                let v = &mut viewers[index];
                v.recovering = recovering;
                v.conn.with(|ob| {
                    ob.data(v.channel, &paint);
                });
            }
        }
        if full_screen && let Some(t) = st.reg.terminals.get_mut(&id) {
            t.full_screen = true;
        }
        if !reports.is_empty() {
            self.report_status(st, id, &reports);
        }
        if let Some(title) = title {
            st.reg.terminals.get_mut(&id).expect("live terminal").title = title.clone();
            self.save_soon();
            self.emit(st, "title", id, serde_json::json!({"title": title}));
        }
    }

    /// Applies 7501 reports and tells subscribers what changed.
    pub fn report_status(&self, st: &mut State, id: u32, reports: &[ghostty_vt::StatusReport]) {
        let word = |st: &State| st.reg.terminals[&id].status.as_ref().map(|s| s.state.clone());
        let mut word_changed = false;
        for r in reports {
            let was = word(st);
            let t = st.reg.terminals.get_mut(&id).expect("live terminal");
            let change = crate::status::apply(t, r);
            self.status_changed(st, id, change);
            word_changed |= word(st) != was;
        }
        // Losing the holder too (a reboot) leaves no tail to replay: queue a new word,
        // and with it a turn or news, for immediate persistence. Messages and progress can wait.
        if word_changed { self.save(st) } else { self.save_soon() }
    }

    pub fn status_changed(&self, st: &mut State, id: u32, change: crate::status::Change) {
        let t = st.reg.terminals.get_mut(&id).expect("reported terminal");
        let first_task_finished = t.first_task_turn.is_some_and(|turn| {
            t.turn > turn
                || (change.root_report
                    && t.status.as_ref().is_some_and(|s| matches!(s.state.as_str(), "done" | "blocked" | "error")))
        });
        if first_task_finished {
            t.first_task_turn = None;
        }
        let (status, records, turn, reported, ctx) =
            (t.status.clone(), t.records.clone(), t.turn, t.reported, t.ctx.clone());
        if change.status {
            self.emit(
                st,
                "status",
                id,
                serde_json::json!({"status": status, "records": records, "reported": reported, "ctx": ctx}),
            );
        }
        if change.turn_ended {
            let state = status.map(|s| s.state);
            self.emit(st, "turn", id, serde_json::json!({"turn": turn, "state": state}));
        }
        if (change.turn_ended || change.stuck || first_task_finished) && st.reg.terminals[&id].is_live() {
            self.news(st, id);
        }
    }

    fn viewer<'a>(st: &'a mut State, conn: &Arc<Conn>, channel: u32) -> Option<(u32, &'a mut Viewer)> {
        st.viewers.iter_mut().find_map(|(id, vs)| {
            vs.iter_mut().find(|v| Arc::ptr_eq(&v.conn, conn) && v.channel == channel).map(|v| (*id, v))
        })
    }

    /// Activity selects the device group and the viewer whose display facts count.
    fn touch(&self, st: &mut State, conn: &Arc<Conn>, channel: u32, f: impl FnOnce(&mut Viewer)) -> Option<u32> {
        let (id, v) = Self::viewer(st, conn, channel)?;
        f(v);
        if v.observe {
            return None;
        }
        v.active_at = Some(Instant::now());
        self.apply_size(st, id);
        Some(id)
    }

    pub fn viewer_input(&self, conn: &Arc<Conn>, channel: u32, bytes: &[u8], automatic: bool) {
        let mut st = self.lock();
        // A terminal's answer to a program's query is not a person: a path
        // that cannot send AUTOMATIC_INPUT (old app, old relay) still sends
        // it as DATA, and it must not count as use (or the idle device would
        // take the grid back each time the program asks).
        // Sizing only: the agent-input gate still sees what the path said.
        let human = !automatic;
        let automatic =
            automatic || Self::viewer(&mut st, conn, channel).is_some_and(|(_, v)| v.replies.only_replies(bytes));
        let id = if automatic {
            Self::viewer(&mut st, conn, channel).filter(|(_, v)| !v.observe).map(|(id, _)| id)
        } else {
            // Empty DATA (a client's keepalive) keeps today's activity meaning
            // but is no one's input.
            self.touch(&mut st, conn, channel, |v| {
                if !bytes.is_empty() {
                    v.input_at = Some(Instant::now());
                }
            })
        };
        if let Some(id) = id {
            let by = Self::viewer(&mut st, conn, channel).map(|(_, v)| crate::input::DraftAuthor {
                connection_id: v.conn.id,
                channel: v.channel,
                origin: v.origin,
                peer: v.peer.clone(),
            });
            let person = !automatic && !bytes.is_empty();
            let held = st.gates.get(&id).is_some_and(|g| g.is_busy());
            if person && let Some(t) = st.reg.terminals.get_mut(&id) {
                // Under the same lock as a guarded send's fence: either the send sees the
                // new `ctx` and refuses, or these bytes are held until it has typed.
                if held { t.bump_ctx_held() } else { t.bump_ctx() }
            }
            let now = st.gates.entry(id).or_default().input(bytes, human, person, by);
            if let Some(link) = st.link(id) {
                link.input(id, &now);
            }
        }
    }

    pub fn viewer_resize(&self, conn: &Arc<Conn>, channel: u32, cols: u16, rows: u16) {
        let mut st = self.lock();
        // The app remeasures after ordered grid paints. Once a negotiated
        // interactive viewer has reported physical geometry, an identical
        // fresh offer is not new activity: touching it could switch sizing
        // between desktop groups and trigger another remeasure. A resumed
        // viewer still passes through this path until its first fresh offer.
        if Self::viewer(&mut st, conn, channel).is_some_and(|(_, v)| {
            !v.observe
                && v.resize_notifications
                && v.visibility_updates
                && v.sized
                && !v.awaiting_size
                && v.size == (cols, rows)
        }) {
            conn.wake.notify_one();
            return;
        }
        self.touch(&mut st, conn, channel, |v| {
            v.size = (cols, rows);
            v.sized = true;
            if cols > 0 && rows > 0 {
                v.awaiting_size = false;
            }
            v.repaint_pending |= !v.resize_notifications && !v.observe && v.desktop_group.is_some();
        });
        conn.wake.notify_one();
    }

    /// Update size eligibility independently from focus or input activity.
    pub fn viewer_visibility(&self, conn: &Arc<Conn>, channel: u32, visible: bool) {
        let mut st = self.lock();
        let id = {
            let Some((id, viewer)) = Self::viewer(&mut st, conn, channel) else { return };
            if !viewer.visibility_updates || viewer.visible == visible {
                return;
            }
            viewer.visible = visible;
            if viewer.observe {
                return;
            }
            if visible {
                // A view returning to the active workspace can lead its desktop
                // group, but it still needs a physical size after a resume.
                viewer.active_at = Some(Instant::now());
            }
            id
        };
        self.apply_size(&mut st, id);
    }

    /// The viewer's window gained or lost keyboard focus (negotiated
    /// WINDOW_FOCUS). The focused window decides the grid; losing focus keeps
    /// it with this viewer until another window is focused.
    pub fn viewer_window_focus(&self, conn: &Arc<Conn>, channel: u32, focused: bool) {
        let mut st = self.lock();
        let id = {
            let Some((id, viewer)) = Self::viewer(&mut st, conn, channel) else { return };
            if !viewer.focus_updates {
                return;
            }
            if focused {
                // Refreshed even when already focused: a repeated report from
                // the window the person just returned to must win.
                viewer.focused_at = Some(Instant::now());
            } else if !viewer.focused {
                return;
            }
            viewer.focused = focused;
            if viewer.observe {
                return;
            }
            id
        };
        self.apply_size(&mut st, id);
    }

    pub fn viewer_facts(&self, conn: &Arc<Conn>, channel: u32, display: Display) {
        let mut st = self.lock();
        let id = {
            let Some((id, v)) = Self::viewer(&mut st, conn, channel) else { return };
            v.display.fg = display.fg.or(v.display.fg.take());
            v.display.bg = display.bg.or(v.display.bg.take());
            v.display.cursor = display.cursor.or(v.display.cursor.take());
            v.display.cell = display.cell.or(v.display.cell);
            id
        };
        let viewers = &st.viewers[&id];
        let selection = select_sizing(viewers, self.settings().size_policy == crate::config::SizePolicy::Grouped);
        let selected_display = selection
            .leader
            .filter(|leader| Arc::ptr_eq(&viewers[*leader].conn, conn) && viewers[*leader].channel == channel)
            .map(|leader| viewers[leader].display.clone());
        if let Some(display) = selected_display {
            self.apply_display(&mut st, id, Some(&display));
        }
    }

    /// Focus changes count as activity, and reach programs that asked for them.
    pub fn viewer_focus(&self, conn: &Arc<Conn>, channel: u32, focused: bool) {
        let mut st = self.lock();
        let id = if focused {
            self.touch(&mut st, conn, channel, |_| {})
        } else {
            Self::viewer(&mut st, conn, channel).filter(|(_, v)| !v.observe).map(|(id, _)| id)
        };
        if let Some(id) = id
            && st.screens.get(&id).is_some_and(|s| s.dec_mode(1004))
            && let Some(link) = st.link(id)
        {
            link.input(id, if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    /// A viewer left (`channel` = None: the whole connection went away).
    pub fn detach(&self, conn: &Arc<Conn>, channel: Option<u32>) {
        let mut st = self.lock();
        self.unsubscribe(&mut st, conn, channel);
        let mut touched = Vec::new();
        let mut left = Vec::new();
        for (id, vs) in st.viewers.iter_mut() {
            let before = vs.len();
            vs.retain(|v| {
                let leaving = Arc::ptr_eq(&v.conn, conn) && channel.is_none_or(|c| c == v.channel);
                if leaving {
                    left.push((*id, v.conn.id, v.channel));
                }
                !leaving
            });
            if vs.len() != before {
                touched.push(*id);
            }
        }
        st.viewers.retain(|_, vs| !vs.is_empty());
        // A draft its typist left behind expires sooner once nobody is left.
        for (id, connection, channel) in left {
            if let Some(gate) = st.gates.get_mut(&id) {
                gate.left(connection, channel);
            }
        }
        for id in touched {
            self.apply_size(&mut st, id);
        }
    }

    /// A local snapshot of attached viewers and the exact selector result used for resizing.
    pub fn viewer_list(&self, id: u32, terminal: String) -> ViewerListResult {
        let mut st = self.lock();
        let draft = draft(&mut st, id);
        let viewers = st.viewers.get(&id).map_or(&[][..], Vec::as_slice);
        let selection = select_sizing(viewers, self.settings().size_policy == crate::config::SizePolicy::Grouped);
        let current = st
            .screens
            .get(&id)
            .map(|screen| screen.size())
            .or_else(|| st.reg.terminals.get(&id).map(|record| (record.cols, record.rows)))
            .unwrap_or_default();
        let identity =
            |index: usize| ViewerIdentity { connection_id: viewers[index].conn.id, channel: viewers[index].channel };
        let now = Instant::now();
        let details = viewers
            .iter()
            .enumerate()
            .map(|(index, viewer)| ViewerInfo {
                identity: identity(index),
                origin: viewer.origin,
                peer: viewer.peer.clone(),
                peer_pid: (viewer.origin != ViewerOrigin::Relayed).then_some(viewer.conn.peer_pid).flatten(),
                offered: GridSize { cols: viewer.size.0, rows: viewer.size.1 },
                visible: viewer.visible,
                size_eligible: selection.candidates.contains(&index),
                visibility_updates: viewer.visibility_updates,
                focus_updates: viewer.focus_updates,
                focused: viewer.focus_updates && viewer.focused,
                focused_ms_ago: viewer
                    .focused_at
                    .map(|focused| now.saturating_duration_since(focused).as_millis() as u64),
                resize_notifications: viewer.resize_notifications,
                observe: viewer.observe,
                sized: viewer.sized,
                awaiting_size: viewer.awaiting_size,
                desktop_group: viewer
                    .desktop_group
                    .as_ref()
                    .map(|(line, group)| ViewerDesktopGroup { line: line.clone(), group: group.clone() }),
                active_ms_ago: viewer.active_at.map(|active| now.saturating_duration_since(active).as_millis() as u64),
                input_ms_ago: viewer.input_at.map(|input| now.saturating_duration_since(input).as_millis() as u64),
                leader: selection.leader == Some(index),
                contributes_to_grid: selection.contributors.contains(&index),
            })
            .collect();
        ViewerListResult {
            terminal,
            current_grid: GridSize { cols: current.0, rows: current.1 },
            selected_grid: selection.size.map(|(cols, rows)| GridSize { cols, rows }),
            focus_authority: selection.focus_authority,
            leader: selection.leader.map(identity),
            viewers: details,
            draft,
        }
    }

    /// Fit the selected desktop window group; display facts follow its sizing leader.
    fn apply_size(&self, st: &mut State, id: u32) {
        let viewers = st.viewers.get(&id).map_or(&[][..], Vec::as_slice);
        let selection = select_sizing(viewers, self.settings().size_policy == crate::config::SizePolicy::Grouped);
        // Keep the holder's inherited grid while legacy relays resume, and while
        // the focused (or last focused) window is hidden or unmeasured. Focus,
        // input and hidden attach placeholders cannot replace a physical offer.
        if selection.leader.is_none() && st.viewers.get(&id).is_some_and(|vs| vs.iter().any(|v| !v.observe)) {
            return;
        }
        let (cols, rows) = selection.size.unwrap_or_else(|| self.headless());
        let display = selection.leader.map(|leader| st.viewers[&id][leader].display.clone());
        if !st.screens.contains_key(&id) {
            return;
        }
        self.apply_display(st, id, display.as_ref());
        let rec = st.reg.terminals.get_mut(&id).expect("live terminal");
        if (rec.cols, rec.rows) == (cols, rows) || cols == 0 || rows == 0 {
            return;
        }
        st.screens.get_mut(&id).expect("live screen").resize(cols, rows);
        rec.cols = cols;
        rec.rows = rows;
        if let Some(link) = st.link(id) {
            link.send(&proto::json_frame(proto::RESIZE, id, &proto::Size { cols, rows }));
        }
        let size = [cols.to_le_bytes(), rows.to_le_bytes()].concat();
        for v in st.viewers.get_mut(&id).into_iter().flatten() {
            if v.resize_notifications {
                v.repaint_pending = true;
                v.conn.with(|out| out.resize(v.channel, &size));
            } else if !v.observe && v.sized && v.desktop_group.is_some() {
                v.repaint_pending = true;
                v.conn.wake.notify_one();
            }
        }
        self.save_soon();
        self.emit(st, "size", id, serde_json::json!({"cols": cols, "rows": rows}));
    }

    /// Serialize only the latest grid, when a viewer has credit to receive it.
    pub fn prepare_repaints(&self, conn: &Arc<Conn>) {
        let mut st = self.lock();
        let ids: Vec<u32> = st.viewers.keys().copied().collect();
        for id in ids {
            self.queue_resize_paints(&mut st, id, Some(conn));
        }
    }

    /// On exit there will be no later writer pass over this screen.
    pub(crate) fn queue_resize_paints(&self, st: &mut State, id: u32, conn: Option<&Arc<Conn>>) {
        let ready = |v: &Viewer| {
            v.repaint_pending && conn.is_none_or(|conn| Arc::ptr_eq(&v.conn, conn) && conn.outbox().can_send(v.channel))
        };
        if !st.viewers.get(&id).into_iter().flatten().any(ready) {
            return;
        }
        // One paint per distinct (`resize_history`, history lost, viewer rows):
        // viewers without history share the screen-only paint, as before, and
        // the history's flush depends on the rows it is rendered in.
        let lost = |v: &Viewer| v.resize_history > 0 && v.conn.outbox().history_lost(v.channel);
        let screen = &st.screens[&id];
        let key = |v: &Viewer| (v.resize_history, lost(v), viewer_rows(v, screen));
        let mut paints: Vec<((usize, bool, u16), (Vec<u8>, bool))> = Vec::new();
        for key in st.viewers[&id].iter().filter(|v| ready(v)).map(key) {
            if !paints.iter().any(|(k, _)| *k == key) {
                paints.push((key, resize_paint_to(st, id, key.0, key.1, key.2)));
            }
        }
        let recovering = st.screens[&id].continuation().is_none();
        let keys: Vec<_> = st.viewers[&id].iter().map(|v| ready(v).then(|| key(v))).collect();
        for (v, key) in st.viewers.get_mut(&id).into_iter().flatten().zip(keys) {
            let Some(key) = key else { continue };
            let (paint, clears) = &paints.iter().find(|(k, _)| *k == key).expect("built above").1;
            // Queued whole, never refused: the RESIZE is always followed by its paint.
            v.conn.with(|out| {
                if !v.observe && v.desktop_group.is_some() {
                    out.repaint_clearing(v.channel, paint, *clears);
                } else {
                    out.paint(v.channel, paint, *clears);
                }
            });
            v.repaint_pending = false;
            v.recovering = recovering;
        }
    }

    fn apply_display(&self, st: &mut State, id: u32, display: Option<&Display>) {
        let Some(screen) = st.screens.get_mut(&id) else { return };
        if let Some(d) = display {
            d.apply(screen);
        }
        // Display changes can notify an idle program, even without a resize or output.
        let replies = Relay::take_responses(screen);
        if let Some(link) = st.link(id) {
            link.input(id, &replies);
        }
    }
}

fn desktop_sizer(v: &Viewer) -> bool {
    !v.observe && v.visible && v.desktop_group.is_some() && v.sized && !v.awaiting_size && v.size.0 > 0 && v.size.1 > 0
}

/// May set the grid at all: interactive, visible and physically measured.
fn measured(v: &Viewer) -> bool {
    !v.observe && v.visible && v.sized && !v.awaiting_size && v.size.0 > 0 && v.size.1 > 0
}

/// Focus-reporting viewers that have been focused or typed into: once any exists, only they
/// may set the grid (the focused window wins; the last focused keeps it).
fn focus_reporter(v: &Viewer) -> bool {
    // Item 71: real input is use like a focus report, so it is history too
    // (a phone attached as never used, then typed into, decides).
    !v.observe && v.focus_updates && (v.focused_at.is_some() || v.input_at.is_some())
}

/// `idle_ms` at or above this (a year) means never used: no focus history at
/// all. Clients send i64::MAX or u64::MAX for "never". (On Linux
/// `Instant::checked_sub` of such an age does not fail: it gives an instant
/// far in the past, which would still be focus history.)
pub(crate) const NEVER_USED_MS: u64 = 365 * 24 * 60 * 60 * 1000;

/// `idle_ms` ago (None: now). At or above [`NEVER_USED_MS`], or older than
/// this machine's clock can say: None, never used.
pub(crate) fn used_ago(idle_ms: Option<u64>) -> Option<Instant> {
    let now = Instant::now();
    match idle_ms {
        None | Some(0) => Some(now),
        Some(ms) if ms >= NEVER_USED_MS => None,
        Some(ms) => now.checked_sub(Duration::from_millis(ms)),
    }
}

/// May decide the grid under focus authority: a focus reporter that was
/// focused, or a phone-like viewer (no desktop group, no focus reports) that
/// a person typed, tapped or pasted into. Legacy desktops (a group, no focus
/// reports) are rule 6's.
fn decider(v: &Viewer) -> bool {
    !v.observe && (focus_reporter(v) || (!v.focus_updates && v.desktop_group.is_none() && v.input_at.is_some()))
}

/// When a decider was last used: a reporter's latest focus or real input; a
/// phone that sends no focus, its latest real input only (a size change,
/// being shown, an empty frame or an attach is not a person using it).
fn last_use(v: &Viewer) -> Option<Instant> {
    if v.focus_updates { v.focused_at.max(v.input_at) } else { v.input_at }
}

/// Latest-used-wins (item 71). The decider used last leads with its own
/// size: a phone being used takes the grid from an idle focused desktop,
/// and the desktop takes it back on its next input or focus. Only a person's
/// input moves it: output, terminal replies, credit and size offers never do.
/// If the latest-used decider is hidden or unmeasured (a phone in a pocket,
/// a window on another workspace), the latest-used focused, visible and
/// measured reporter leads (the window Wolf is looking at); else the grid is
/// held (a hidden desktop used after the phone keeps its grid).
fn select_focused(viewers: &[Viewer], grouped: bool) -> SizeSelection {
    let deciders: Vec<usize> = (0..viewers.len()).filter(|i| decider(&viewers[*i])).collect();
    let candidates: Vec<usize> = deciders.iter().copied().filter(|i| measured(&viewers[*i])).collect();
    // Ties (one report touching every pane of a window): focused and visible first.
    let rank = |i: &usize| {
        let v = &viewers[*i];
        (last_use(v), v.focused && v.visible)
    };
    let latest = deciders.iter().copied().max_by_key(rank);
    let leader = match latest {
        Some(i) if measured(&viewers[i]) => Some(i),
        _ => {
            candidates.iter().copied().filter(|i| focus_reporter(&viewers[*i]) && viewers[*i].focused).max_by_key(rank)
        }
    };
    let Some(leader) = leader else {
        return SizeSelection { candidates, focus_authority: true, ..Default::default() };
    };
    let selected = &viewers[leader];
    let contributors: Vec<usize> = if grouped && selected.focused && selected.desktop_group.is_some() {
        candidates
            .iter()
            .copied()
            .filter(|i| viewers[*i].focused && viewers[*i].desktop_group == selected.desktop_group)
            .collect()
    } else {
        vec![leader]
    };
    let size = contributors
        .iter()
        .map(|index| viewers[*index].size)
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1)))
        .unwrap_or(selected.size);
    SizeSelection { candidates, leader: Some(leader), contributors, size: Some(size), focus_authority: true }
}

#[derive(Default)]
struct SizeSelection {
    /// Focus-reporting viewers decide (see `select_focused`).
    focus_authority: bool,
    candidates: Vec<usize>,
    leader: Option<usize>,
    contributors: Vec<usize>,
    size: Option<(u16, u16)>,
}

fn select_sizing(viewers: &[Viewer], grouped: bool) -> SizeSelection {
    // Focus history rules while a reporter that has it is shown. When every
    // such reporter is hidden, a visible measured desktop whose path could not
    // negotiate focus (old app, old relay) leads under the previous rules;
    // reporting windows that were never focused never lead, and a phone leads
    // only once used after the hidden window was last focused (select_focused).
    let reporting = viewers.iter().any(focus_reporter);
    let shown_reporter = viewers.iter().any(|v| focus_reporter(v) && v.visible);
    let eligible = |v: &Viewer| !(reporting && v.focus_updates);
    if reporting && (shown_reporter || !viewers.iter().any(|v| desktop_sizer(v) && eligible(v))) {
        return select_focused(viewers, grouped);
    }
    let awaiting_size = viewers.iter().any(|v| !v.observe && v.visible && v.awaiting_size);
    // A measured visible desktop owns the sizing device. Phone and legacy
    // viewers are fallback leaders only until such a desktop is available.
    let desktop_available = viewers.iter().any(desktop_sizer);
    let candidates: Vec<usize> = viewers
        .iter()
        .enumerate()
        .filter_map(|(index, v)| {
            (!v.observe
                && eligible(v)
                && v.visible
                && !v.awaiting_size
                && (!awaiting_size || v.sized)
                && if desktop_available { desktop_sizer(v) } else { v.desktop_group.is_none() })
            .then_some(index)
        })
        .collect();
    let Some(leader) = candidates.iter().copied().max_by_key(|index| viewers[*index].active_at) else {
        return SizeSelection { candidates, ..Default::default() };
    };
    let selected = &viewers[leader];
    let (contributors, size) = if grouped && selected.desktop_group.is_some() {
        let contributors: Vec<usize> = viewers
            .iter()
            .enumerate()
            .filter_map(|(index, v)| {
                (!v.observe
                    && eligible(v)
                    && v.desktop_group == selected.desktop_group
                    && v.visible
                    && v.sized
                    && !v.awaiting_size
                    && v.size.0 > 0
                    && v.size.1 > 0
                    && v.active_at.is_some())
                .then_some(index)
            })
            .collect();
        let size = contributors
            .iter()
            .map(|index| viewers[*index].size)
            .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1)))
            .unwrap_or(selected.size);
        (if contributors.is_empty() { vec![leader] } else { contributors }, size)
    } else {
        (vec![leader], selected.size)
    };
    SizeSelection { candidates, leader: Some(leader), contributors, size: Some(size), ..Default::default() }
}

#[cfg(test)]
fn viewer_size(viewers: &[Viewer], grouped: bool) -> (Option<&Viewer>, Option<(u16, u16)>) {
    let selection = select_sizing(viewers, grouped);
    (selection.leader.map(|index| &viewers[index]), selection.size)
}

/// The screen, then the latest 7501 report of every record and whether the
/// program wants win32 key events (both pass through to viewers, so a new one
/// must see them too; a viewer repainted after falling behind gets the mode
/// either way).
pub(crate) fn first_paint(st: &State, id: u32, history: usize) -> Vec<u8> {
    first_paint_for(st, id, history, st.screens[&id].size().1)
}

/// [`first_paint`] for a viewer that renders `rows` rows ([`viewer_rows`]):
/// its history goes exactly into that viewer's scrollback.
pub(crate) fn first_paint_for(st: &State, id: u32, history: usize, rows: u16) -> Vec<u8> {
    let screen = &st.screens[&id];
    finish_paint(st, id, screen.paint_for(history, rows))
}

/// The rows a viewer renders this screen in: the PTY's for one told of every
/// size (a RESIZE goes before each paint), otherwise its own reported rows (an
/// `hc attach`, which can be taller or shorter than the PTY it does not size).
fn viewer_rows(v: &Viewer, screen: &ghostty_vt::Terminal) -> u16 {
    if v.resize_notifications || v.size.1 == 0 { screen.size().1 } else { v.size.1 }
}

/// [`resize_paint_to`] for a viewer with the PTY's rows.
#[cfg(test)]
pub(crate) fn resize_paint_for(st: &State, id: u32, history: usize, force: bool) -> (Vec<u8>, bool) {
    resize_paint_to(st, id, history, force, st.screens[&id].size().1)
}

/// The repaint after a RESIZE for a viewer that asked for `history` rows of
/// it and renders `rows` rows ([`viewer_rows`]), and whether it clears the
/// viewer's history. With `history > 0` on the main screen it is CAN, the
/// screen's reset (ending `ESC[H ESC[2J`), then
/// `ESC[3J`, so the history that follows replaces the viewer's, then the
/// screen. Only when all of it fits [`RESIZE_PAINT_BUDGET`] (all `history`
/// rows, or every row there is): otherwise, and on the alternate screen
/// (whose paint carries no history), it is the screen-only paint without
/// `ESC[3J`, so the viewer keeps its own history rather than losing rows.
/// `force`: a history-clearing paint was cut short, so the viewer's history
/// may be gone; it gets the whole history whatever the size. 0: exactly
/// [`first_paint`] without history.
fn resize_paint_to(st: &State, id: u32, history: usize, force: bool, rows: u16) -> (Vec<u8>, bool) {
    let screen = &st.screens[&id];
    if history > 0 && !screen.alt_screen() {
        let mut painted = screen.paint_for(history.min(MAX_RESIZE_HISTORY), rows);
        let at = ghostty_vt::PAINT_RESET.len();
        debug_assert!(painted.starts_with(ghostty_vt::PAINT_RESET));
        painted.splice(at..at, *b"\x1b[3J");
        let paint = finish_paint(st, id, painted);
        if force || paint.len() <= RESIZE_PAINT_BUDGET {
            return (paint, true);
        }
    }
    (first_paint(st, id, 0), false)
}

/// [`resize_paint_to`]'s bytes for a viewer with the PTY's rows, not forced.
pub(crate) fn resize_paint(st: &State, id: u32, history: usize) -> Vec<u8> {
    resize_paint_to(st, id, history, false, st.screens[&id].size().1).0
}

/// Most history rows a resize paint carries (`resize_history` above it is this).
pub const MAX_RESIZE_HISTORY: usize = 100_000;
/// Largest history-replacing resize paint: half the stream backlog limit
/// ([`crate::wire::MAX_PENDING`]), so output that follows it still fits.
pub const RESIZE_PAINT_BUDGET: usize = crate::wire::MAX_PENDING / 2;

fn finish_paint(st: &State, id: u32, painted: Vec<u8>) -> Vec<u8> {
    #[cfg(test)]
    tests::PAINTS.with(|n| n.set(n.get() + 1));
    let screen = &st.screens[&id];
    let mut paint = paint_state(st, id, painted);
    if let Some(continuation) = screen.continuation() {
        paint.extend(continuation);
    }
    paint
}

fn paint_state(st: &State, id: u32, screen: Vec<u8>) -> Vec<u8> {
    // A backlog may have ended in any parser state, not at an escape boundary.
    let mut paint = vec![0x18];
    paint.extend(screen);
    let t = &st.reg.terminals[&id];
    paint.extend(if t.win32_input { b"\x1b[?9001h" } else { b"\x1b[?9001l" });
    let root = t.status.iter().map(|s| ("", s));
    for (rid, status) in root.chain(t.records.iter().map(|(k, s)| (k.as_str(), s))) {
        paint.extend(status.encode(rid).as_bytes());
    }
    paint
}
