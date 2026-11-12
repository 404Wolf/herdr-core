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
use std::time::Instant;

#[cfg(test)]
mod focus_tests;
#[cfg(test)]
mod sizing_tests;
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
    focus_updates: bool,
    focused: bool,
    /// One owner per terminal; cleared on other viewers when focus/input claims it.
    focus_leader: bool,
    /// Frozen on blur/hidden; physical reoffers from followers cannot change it.
    focus_grid: Option<(u16, u16)>,
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
    /// Effective visibility; legacy clients remain visible until detach. Sticky focus ownership can remain hidden.
    pub visible: bool,
    /// Candidate in the current sizing selector (can be false while visible).
    pub size_eligible: bool,
    pub visibility_updates: bool,
    /// The attach path negotiated focused desktop sizing.
    pub focus_updates: bool,
    /// Last reported active-window/pane focus state, independent of sticky leadership.
    pub focused: bool,
    pub resize_notifications: bool,
    pub observe: bool,
    pub sized: bool,
    pub awaiting_size: bool,
    pub desktop_group: Option<ViewerDesktopGroup>,
    /// Milliseconds since last focus/input/size/visibility activity.
    pub active_ms_ago: Option<u64>,
    pub leader: bool,
    /// The sole selected viewer whose offer or sticky focus grid sets the size.
    pub contributes_to_grid: bool,
}

#[derive(Serialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct ViewerListResult {
    pub terminal: String,
    pub current_grid: GridSize,
    /// What the same selector used by the live resizer currently chooses.
    pub selected_grid: Option<GridSize>,
    pub leader: Option<ViewerIdentity>,
    pub viewers: Vec<ViewerInfo>,
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
    pub size: (u16, u16),
    pub display: Display,
    pub resumed: bool,
    pub visible: bool,
    pub visibility_updates: bool,
    pub focus_updates: bool,
    pub focused: bool,
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
        let focus_updates = req.focus_updates && req.desktop_group.is_some();
        let channel = conn.new_channel();
        let (origin, peer) = viewer_origin(&st, conn);
        let recovering = st.screens[&id].continuation().is_none();
        st.viewers.entry(id).or_default().push(Viewer {
            conn: conn.clone(),
            channel,
            observe: req.observe,
            resize_notifications: req.resize_notifications,
            repaint_pending: false,
            size: req.size,
            active_at: (!req.resumed).then(Instant::now),
            sized: false,
            visible: req.visible,
            awaiting_size: req.resumed,
            visibility_updates: req.visibility_updates,
            focus_updates,
            focused: focus_updates && req.focused,
            focus_leader: false,
            focus_grid: None,
            origin,
            peer,
            desktop_group: req.desktop_group,
            display: req.display,
            recovering,
        });
        if focus_updates && req.focused {
            Self::claim_focus(&mut st, id, conn, channel);
        }
        self.apply_size(&mut st, id);
        let paint = first_paint(&st, id, req.history);
        st.viewers.get_mut(&id).unwrap().last_mut().unwrap().repaint_pending = false;
        conn.with(|out| {
            out.open_paused(channel);
            if req.resize_notifications {
                let t = &st.reg.terminals[&id];
                let size = [t.cols.to_le_bytes(), t.rows.to_le_bytes()].concat();
                out.resize(channel, &size);
            }
            out.data(channel, &paint);
        });
        Ok(channel)
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
        for (index, v) in st.viewers.get_mut(&id).into_iter().flatten().enumerate().filter(|_| !scrolling) {
            if v.repaint_pending {
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
        let (status, records, turn, reported) = (t.status.clone(), t.records.clone(), t.turn, t.reported);
        if change.status {
            self.emit(
                st,
                "status",
                id,
                serde_json::json!({"status": status, "records": records, "reported": reported}),
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

    /// Only an explicit desktop focus/input signal transfers ownership.
    fn claim_focus(st: &mut State, id: u32, conn: &Arc<Conn>, channel: u32) {
        let Some(viewers) = st.viewers.get_mut(&id) else { return };
        if !viewers.iter().any(|v| {
            Arc::ptr_eq(&v.conn, conn)
                && v.channel == channel
                && !v.observe
                && v.focus_updates
                && v.desktop_group.is_some()
                && v.visible
        }) {
            return;
        }
        for v in viewers {
            v.focus_leader = Arc::ptr_eq(&v.conn, conn) && v.channel == channel;
            if v.focus_leader && v.sized && !v.awaiting_size && v.size.0 > 0 && v.size.1 > 0 {
                v.focus_grid = Some(v.size);
            }
        }
    }

    /// Activity selects the device group and the viewer whose display facts count.
    fn touch(&self, st: &mut State, conn: &Arc<Conn>, channel: u32, f: impl FnOnce(&mut Viewer)) -> Option<u32> {
        let (id, v) = Self::viewer(st, conn, channel)?;
        f(v);
        if v.observe {
            return None;
        }
        v.active_at = Some(Instant::now());
        if v.focus_leader && v.focused && v.visible && v.sized && !v.awaiting_size && v.size.0 > 0 && v.size.1 > 0 {
            v.focus_grid = Some(v.size);
        }
        self.apply_size(st, id);
        Some(id)
    }

    pub fn viewer_input(&self, conn: &Arc<Conn>, channel: u32, bytes: &[u8], automatic: bool) {
        let mut st = self.lock();
        let id = if automatic {
            Self::viewer(&mut st, conn, channel).filter(|(_, v)| !v.observe).map(|(id, _)| id)
        } else {
            if let Some((id, _)) = Self::viewer(&mut st, conn, channel) {
                Self::claim_focus(&mut st, id, conn, channel);
            }
            self.touch(&mut st, conn, channel, |_| {})
        };
        if let Some(id) = id {
            let now = st.gates.entry(id).or_default().viewer(bytes, !automatic);
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
            if let Some((id, _)) = Self::viewer(&mut st, conn, channel) {
                Self::claim_focus(&mut st, id, conn, channel);
            }
            self.touch(&mut st, conn, channel, |v| v.focused = v.focus_updates)
        } else {
            Self::viewer(&mut st, conn, channel).filter(|(_, v)| !v.observe).map(|(id, v)| {
                v.focused = false;
                id
            })
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
        for (id, vs) in st.viewers.iter_mut() {
            let before = vs.len();
            vs.retain(|v| !(Arc::ptr_eq(&v.conn, conn) && channel.is_none_or(|c| c == v.channel)));
            if vs.len() != before {
                touched.push(*id);
            }
        }
        st.viewers.retain(|_, vs| !vs.is_empty());
        for id in touched {
            self.apply_size(&mut st, id);
        }
    }

    /// A local snapshot of attached viewers and the exact selector result used for resizing.
    pub fn viewer_list(&self, id: u32, terminal: String) -> ViewerListResult {
        let st = self.lock();
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
                focused: viewer.focused,
                resize_notifications: viewer.resize_notifications,
                observe: viewer.observe,
                sized: viewer.sized,
                awaiting_size: viewer.awaiting_size,
                desktop_group: viewer
                    .desktop_group
                    .as_ref()
                    .map(|(line, group)| ViewerDesktopGroup { line: line.clone(), group: group.clone() }),
                active_ms_ago: viewer.active_at.map(|active| now.saturating_duration_since(active).as_millis() as u64),
                leader: selection.leader == Some(index),
                contributes_to_grid: selection.contributors.contains(&index),
            })
            .collect();
        ViewerListResult {
            terminal,
            current_grid: GridSize { cols: current.0, rows: current.1 },
            selected_grid: selection.size.map(|(cols, rows)| GridSize { cols, rows }),
            leader: selection.leader.map(identity),
            viewers: details,
        }
    }

    /// Apply the focused/sticky leader or fallback offer and its display facts.
    fn apply_size(&self, st: &mut State, id: u32) {
        let viewers = st.viewers.get(&id).map_or(&[][..], Vec::as_slice);
        let selection = select_sizing(viewers, self.settings().size_policy == crate::config::SizePolicy::Grouped);
        // Keep the holder's inherited grid while legacy relays resume. Focus,
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
        let paint = first_paint(st, id, 0);
        let recovering = st.screens[&id].continuation().is_none();
        for v in st.viewers.get_mut(&id).into_iter().flatten().filter(|v| ready(v)) {
            v.conn.with(|out| {
                if !v.observe && v.desktop_group.is_some() {
                    out.repaint(v.channel, &paint);
                } else {
                    out.data(v.channel, &paint);
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

#[derive(Default)]
struct SizeSelection {
    candidates: Vec<usize>,
    leader: Option<usize>,
    contributors: Vec<usize>,
    size: Option<(u16, u16)>,
}

fn select_sizing(viewers: &[Viewer], _grouped: bool) -> SizeSelection {
    // The focus contract reserves sizing even before the first focus/measurement.
    // No legacy/mobile fallback may resize a desktop that has negotiated it.
    if viewers.iter().any(|v| !v.observe && v.focus_updates && v.desktop_group.is_some()) {
        let candidates: Vec<usize> = viewers
            .iter()
            .enumerate()
            .filter_map(|(index, v)| {
                (!v.observe && v.focus_updates && v.focus_leader && v.focus_grid.is_some()).then_some(index)
            })
            .collect();
        let leader = candidates.first().copied();
        return SizeSelection {
            size: leader.and_then(|index| viewers[index].focus_grid),
            contributors: leader.into_iter().collect(),
            candidates,
            leader,
        };
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
    let contributors = vec![leader];
    let size = selected.size;
    SizeSelection { candidates, leader: Some(leader), contributors, size: Some(size) }
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
    #[cfg(test)]
    tests::PAINTS.with(|n| n.set(n.get() + 1));
    let screen = &st.screens[&id];
    let mut paint = paint_state(st, id, screen.paint(history));
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
