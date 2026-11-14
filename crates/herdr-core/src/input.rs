//! Input from agents: one queue per terminal, so send, keys and interrupt
//! never interleave with each other, and never with a person typing.

use crate::holder_proto as proto;
use crate::rpc::RpcError;
use crate::runtime::{Core, State};
use crate::terminals::Terminal;
use std::time::{Duration, Instant};

/// Pause between the paste and Enter, so the program sees them apart.
pub const PASTE_GAP: Duration = Duration::from_millis(200);
/// How often a send waiting on a person's draft looks again.
const DRAFT_POLL: Duration = Duration::from_millis(100);
/// Pause between keys, so ESC then x isn't read as alt+x.
const KEY_GAP: Duration = Duration::from_millis(30);
/// A program whose output stopped this long ago has finished drawing (`ready: quiet`).
pub const OUTPUT_SETTLE: Duration = Duration::from_millis(800);
/// How long after start an agent may still be setting itself up.
const STARTUP_MS: u64 = 10_000;
/// The longest a send waits for a starting agent to settle.
const SETTLE_MAX: Duration = Duration::from_secs(5);

/// A person's typing, as it affects agent input to one terminal.
#[derive(Default)]
pub struct Gate {
    last_human: Option<Instant>,
    /// An agent's input is going in right now: viewer input waits.
    busy: bool,
    held: Vec<u8>,
    held_human: bool,
    /// A person typed text they haven't sent or cleared yet (see `drafting`).
    draft: bool,
}

impl Gate {
    /// A person pressed keys while an agent's input was going in.
    pub fn has_held(&self) -> bool {
        self.held_human
    }

    /// Starts or ends an agent's input; ending returns the viewer bytes held meanwhile.
    pub fn set_busy(&mut self, busy: bool) -> Vec<u8> {
        self.busy = busy;
        if busy {
            Vec::new()
        } else {
            self.held_human = false;
            std::mem::take(&mut self.held)
        }
    }

    /// Viewer bytes: straight through, or held while an agent types.
    /// Only human input resets the quiet period.
    pub fn viewer(&mut self, bytes: &[u8], human: bool) -> Vec<u8> {
        if human {
            self.last_human = Some(Instant::now());
            self.draft = drafting(self.draft, bytes);
        }
        if self.busy {
            self.held_human |= human && !bytes.is_empty();
            self.held.extend_from_slice(bytes);
            Vec::new()
        } else {
            bytes.to_vec()
        }
    }
}

/// Whether a person's keys leave a draft in the program's input: text starts
/// one; Enter, ctrl+c or ctrl+u ends it (plain or as CSI u keys). Other
/// sequences (arrows, mouse, focus) change nothing. Backspacing a draft away
/// still counts as one: when unsure, an agent's text waits.
pub fn drafting(mut draft: bool, bytes: &[u8]) -> bool {
    let mut paste = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' | 0x03 | 0x15 if !paste => draft = false,
            0x1b => {
                let (len, csi) = escape(&bytes[i..]);
                match csi {
                    Some((b"200", b'~')) => paste = true,
                    Some((b"201", b'~')) => paste = false,
                    Some((params, b'u')) => draft = csi_key(draft, params),
                    _ => {}
                }
                i += len;
                continue;
            }
            c if c >= 0x20 && c != 0x7f => draft = true,
            _ => {}
        }
        i += 1;
    }
    draft
}

/// The length of the escape sequence `bytes` starts with, and for a CSI its
/// parameters and final byte.
fn escape(bytes: &[u8]) -> (usize, Option<(&[u8], u8)>) {
    match bytes.get(1) {
        Some(b'[') => {
            let end = bytes[2..].iter().position(|b| (0x40..=0x7e).contains(b));
            match end {
                // X10 mouse: three raw bytes follow `ESC [ M`.
                Some(0) if bytes[2] == b'M' => ((6).min(bytes.len()), None),
                Some(n) => (n + 3, Some((&bytes[2..2 + n], bytes[2 + n]))),
                None => (bytes.len(), None),
            }
        }
        Some(b']') => {
            let end = bytes.windows(2).position(|w| w[0] == 0x07 || w == b"\x1b\\");
            end.map_or((bytes.len(), None), |n| (n + if bytes[n] == 0x07 { 1 } else { 2 }, None))
        }
        Some(b'O') => ((3).min(bytes.len()), None),
        Some(_) => (2, None),
        None => (1, None),
    }
}

/// A key in the CSI u (kitty) encoding: `codepoint[;modifiers]`.
fn csi_key(draft: bool, params: &[u8]) -> bool {
    let text = std::str::from_utf8(params).unwrap_or("");
    let mut parts = text.split(';').map(|p| p.split(':').next().unwrap_or(""));
    let key: u32 = parts.next().and_then(|k| k.parse().ok()).unwrap_or(0);
    let mods: u32 = parts.next().and_then(|m| m.parse().ok()).unwrap_or(1);
    match (key, mods) {
        (13, 1) | (99 | 117, 5) => false,
        (32.., 1 | 2) if key != 127 && !(57344..=63743).contains(&key) => true,
        _ => draft,
    }
}

/// How long an agent's text waits for a person's draft to be sent or cleared.
fn draft_max() -> Duration {
    env_ms("HC_DRAFT_MAX_MS", 120_000)
}

pub enum Step {
    FirstTask(Vec<u8>),
    Write(Vec<u8>),
    Pause(Duration),
}

fn env_ms(var: &str, default: u64) -> Duration {
    Duration::from_millis(std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default))
}

/// How long a person must have stopped typing before an agent's send goes in.
pub fn quiet() -> Duration {
    env_ms("HC_QUIET_MS", 3000)
}

/// How long `send` watches for the program to start working.
fn started_window() -> Duration {
    env_ms("HC_STARTED_MS", 5000)
}

fn working(t: &Terminal) -> bool {
    t.status.as_ref().is_some_and(|s| s.state == "working")
}

pub(crate) fn state_word(t: &Terminal) -> Option<String> {
    t.status.as_ref().map(|s| s.state.clone())
}

/// `--if`: the status must be exactly `want`, or nothing goes in.
pub(crate) fn check_status(id: u32, word: Option<&str>, want: Option<&str>) -> Result<(), RpcError> {
    match want {
        Some(want) if word != Some(want) => Err(RpcError::new(
            "status_mismatch",
            format!("{} is {}, not {want}", crate::terminals::address(id), word.unwrap_or("silent")),
            "nothing was typed",
        )),
        _ => Ok(()),
    }
}

/// A paste the way a terminal sends one: bracketed when the program asked.
pub fn paste(bracketed: bool, text: &str) -> Vec<u8> {
    let text = text.replace("\x1b[201~", "");
    if bracketed { format!("\x1b[200~{text}\x1b[201~").into_bytes() } else { text.into_bytes() }
}

pub struct Sent {
    pub status: Option<String>,
    pub turn: u64,
    /// Whether it went `working` soon after; None when it never reports status.
    pub started: Option<bool>,
    /// `typed`, or `inbox` when its program follows its inbox (inbox.rs).
    pub via: &'static str,
    /// Queued: messages now waiting in its inbox.
    pub waiting: Option<usize>,
}

pub enum Interrupted {
    Stopped,
    Exited,
    StillWorking,
    Unknown,
}

impl Core {
    /// Types `steps` (built by `prepare` from the live state) into terminal `id`.
    /// `quiet`: how long a person must have stopped typing first (zero: don't wait).
    /// `text`: it types text, which must never join a person's draft: it waits
    /// for them to send or clear it, and refuses after `draft_max`.
    pub async fn type_in<T>(
        &self,
        id: u32,
        quiet: Duration,
        text: bool,
        prepare: impl FnOnce(&State, &Terminal) -> Result<(T, Vec<Step>), RpcError>,
    ) -> Result<T, RpcError> {
        let queue = self.lock().queues.entry(id).or_default().clone();
        let _turn = queue.lock().await;
        let give_up = Instant::now() + draft_max();
        loop {
            let (left, draft) = {
                let st = self.lock();
                let gate = st.gates.get(&id);
                let left = gate.and_then(|g| g.last_human).map(|t| quiet.saturating_sub(t.elapsed()));
                (left, text && gate.is_some_and(|g| g.draft))
            };
            match left {
                Some(left) if !left.is_zero() => tokio::time::sleep(left).await,
                _ if draft && Instant::now() < give_up => tokio::time::sleep(DRAFT_POLL).await,
                _ if draft => {
                    return Err(RpcError::new(
                        "drafting",
                        format!("{} has a draft a person hasn't sent", crate::terminals::address(id)),
                        "nothing was typed; try again once they send or clear it",
                    ));
                }
                _ => break,
            }
        }
        let (out, steps) = {
            let mut st = self.lock();
            let t = st.reg.terminals.get(&id).filter(|t| t.is_live()).ok_or_else(|| {
                RpcError::new(
                    "finished",
                    format!("{} has finished", crate::terminals::address(id)),
                    format!("hc read {}", crate::terminals::address(id)),
                )
            })?;
            t.check_program()?;
            let prepared = prepare(&st, t)?;
            st.gates.entry(id).or_default().busy = true;
            prepared
        };
        for step in steps {
            match step {
                Step::FirstTask(bytes) => {
                    let mut st = self.lock();
                    if st.link(id).is_some() {
                        if let Some(t) = st.reg.terminals.get_mut(&id) {
                            t.first_task_turn = Some(t.turn);
                        }
                        st.link(id).expect("delivery link").input(id, &bytes);
                        self.save(&st);
                        self.changed.notify_waiters();
                    }
                }
                Step::Write(bytes) => {
                    if let Some(link) = self.lock().link(id) {
                        link.input(id, &bytes);
                    }
                }
                Step::Pause(d) => tokio::time::sleep(d).await,
            }
        }
        let mut st = self.lock();
        let gate = st.gates.entry(id).or_default();
        let held = gate.set_busy(false);
        if let Some(link) = st.link(id) {
            link.input(id, &held);
        }
        Ok(out)
    }

    /// Paste + Enter, after a person has paused typing. Refuses a blocked
    /// program (typing would answer its dialog) unless `force`.
    pub async fn send(&self, id: u32, text: &str, force: bool, only_if: Option<&str>) -> Result<Sent, RpcError> {
        self.send_message(id, text, force, only_if, false).await
    }

    pub(crate) async fn send_first_task(&self, id: u32, text: &str) -> Result<Sent, RpcError> {
        self.send_message(id, text, false, None, true).await
    }

    async fn send_message(
        &self,
        id: u32,
        text: &str,
        force: bool,
        only_if: Option<&str>,
        first_task: bool,
    ) -> Result<Sent, RpcError> {
        if let Some(t) = self.lock().reg.terminals.get(&id) {
            t.check_program()?;
        }
        self.settle(id).await?;
        let (status, turn, reported) = self
            .type_in(id, quiet(), true, |st, t| {
                let word = state_word(t);
                if word.as_deref() == Some("blocked") && !force {
                    return Err(RpcError::new(
                        "blocked",
                        format!(
                            "{} is blocked ({})",
                            crate::terminals::address(id),
                            t.status.as_ref().and_then(|s| s.kind.as_deref()).unwrap_or("waiting")
                        ),
                        format!(
                            "typing would answer its dialog; read its screen first (hc read {}), or --force",
                            crate::terminals::address(id)
                        ),
                    ));
                }
                check_status(id, word.as_deref(), only_if)?;
                let screen = st.screens.get(&id).expect("live terminal has a screen");
                // A template may say how its agent submits (default: Enter after 200 ms).
                let submit = match &t.submit {
                    Some(key) => crate::keys::encode(screen, key)?,
                    None => b"\r".to_vec(),
                };
                let gap = t.paste_gap_ms.map_or(PASTE_GAP, Duration::from_millis);
                let bytes = paste(screen.dec_mode(2004), text);
                let paste = if first_task { Step::FirstTask(bytes) } else { Step::Write(bytes) };
                let steps = vec![paste, Step::Pause(gap), Step::Write(submit)];
                Ok(((word, t.turn, t.reported), steps))
            })
            .await?;
        let started = self.started_after(id, turn, reported).await;
        Ok(Sent { status, turn, started, via: "typed", waiting: None })
    }

    /// Whether `id` went to work after `turn` within a few seconds; None when it never reports status.
    pub(crate) async fn started_after(&self, id: u32, turn: u64, reported: bool) -> Option<bool> {
        if !reported {
            return None;
        }
        let went = |st: &State| st.reg.terminals.get(&id).is_none_or(|t| t.turn > turn || working(t) || !t.is_live());
        self.wait_for(started_window(), went).await;
        let st = self.lock();
        Some(st.reg.terminals.get(&id).is_some_and(|t| t.turn > turn || working(t)))
    }

    /// A starting agent can report before it is ready for a paste (Pi swaps its
    /// input box right after its first report and drops a paste in flight):
    /// in its first seconds, wait until its screen has been still a moment.
    async fn settle(&self, id: u32) -> Result<(), RpcError> {
        let deadline = Instant::now() + SETTLE_MAX;
        loop {
            let ready = {
                let st = self.lock();
                let age = st
                    .reg
                    .terminals
                    .get(&id)
                    .map(|t| Duration::from_millis(crate::runtime::now_ms().saturating_sub(t.created_ms)));
                age.is_none_or(|age| {
                    age >= Duration::from_millis(STARTUP_MS)
                        || st.last_output.get(&id).map_or(age, Instant::elapsed) >= env_ms("HC_SETTLE_MS", 800)
                })
            };
            if ready {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(RpcError::new(
                    "unavailable",
                    format!("{} is still starting: its output has not settled", crate::terminals::address(id)),
                    format!("nothing was typed; check hc read {}, then retry", crate::terminals::address(id)),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Named keys, after a person has paused typing.
    pub async fn keys(&self, id: u32, names: &[String]) -> Result<(Option<String>, u64), RpcError> {
        self.type_in(id, quiet(), false, |st, t| {
            let screen = st.screens.get(&id).expect("live terminal has a screen");
            let mut steps = Vec::new();
            for name in names {
                steps.push(Step::Write(crate::keys::encode(screen, name)?));
                steps.push(Step::Pause(KEY_GAP));
            }
            Ok(((state_word(t), t.turn), steps))
        })
        .await
    }

    /// Esc for programs that report status, else ctrl+c; then watch what happens.
    pub async fn interrupt(&self, id: u32, timeout: Duration) -> Result<(Interrupted, String), RpcError> {
        let (reported, pid) = {
            let st = self.lock();
            let t = st.reg.terminals.get(&id).ok_or_else(|| RpcError::new("not_found", "terminal removed", ""))?;
            t.check_program()?;
            (t.reported, t.pid.unwrap_or(0) as i32)
        };
        let before = if reported { -1 } else { self.foreground(id).await };
        let key = self
            .type_in(id, Duration::ZERO, false, |st, t| {
                let default = if reported { "esc" } else { "ctrl+c" };
                let key = t.stop.clone().unwrap_or_else(|| default.into());
                let screen = st.screens.get(&id).expect("live terminal has a screen");
                let bytes = crate::keys::encode(screen, &key)?;
                Ok((key, vec![Step::Write(bytes)]))
            })
            .await?;
        let live = |st: &State| st.reg.terminals.get(&id).is_some_and(|t| t.is_live());
        if reported {
            let settled = |st: &State| st.reg.terminals.get(&id).is_none_or(|t| !t.is_live() || !working(t));
            self.wait_for(timeout, settled).await;
            let st = self.lock();
            let outcome = match st.reg.terminals.get(&id) {
                Some(t) if !t.is_live() => Interrupted::Exited,
                Some(t) if working(t) => Interrupted::StillWorking,
                _ => Interrupted::Stopped,
            };
            return Ok((outcome, key));
        }
        // No status reports: a shell that gets the terminal back has stopped its job.
        let deadline = Instant::now() + timeout;
        loop {
            if !live(&self.lock()) {
                return Ok((Interrupted::Exited, key));
            }
            if before != pid && self.foreground(id).await == pid {
                return Ok((Interrupted::Stopped, key));
            }
            if Instant::now() >= deadline {
                return Ok((Interrupted::Unknown, key));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The process group in the foreground of terminal `id`, or -1.
    pub(crate) async fn foreground(&self, id: u32) -> i32 {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut st = self.lock();
            st.foreground.entry(id).or_default().push(tx);
            let Some(link) = st.link(id) else { return -1 };
            link.send(&proto::frame(proto::FOREGROUND, id, &[]));
        }
        tokio::time::timeout(Duration::from_secs(1), rx).await.ok().and_then(Result::ok).unwrap_or(-1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_replies_preserve_the_human_quiet_gate() {
        let mut gate = Gate::default();
        assert_eq!(gate.viewer(b"reply", false), b"reply");
        assert!(gate.last_human.is_none());
        gate.viewer(b"x", true);
        let human = gate.last_human;
        gate.set_busy(true);
        assert!(gate.viewer(b"automatic", false).is_empty());
        assert!(!gate.has_held(), "automatic replies are not a person interrupting a scroll");
        assert_eq!(gate.last_human, human);
        gate.viewer(b"typed", true);
        assert!(gate.has_held());
        assert_eq!(gate.set_busy(false), b"automatictyped");
        assert!(!gate.has_held());
    }

    #[test]
    fn a_draft_starts_with_text_and_ends_when_sent_or_cleared() {
        let d = |bytes: &[u8]| drafting(false, bytes);
        assert!(d(b"half a thought"));
        assert!(!d(b"ls\r"), "sent");
        assert!(!d(b"oops\x15") && !d(b"oops\x03"), "cleared: ctrl+u, ctrl+c");
        // Keys as CSI u (kitty keyboard): Enter, ctrl+c, ctrl+u end it; text starts one.
        assert!(!d(b"x\x1b[13u") && !d(b"x\x1b[99;5u") && !d(b"x\x1b[117;5u"));
        assert!(d(b"\x1b[97u") && drafting(true, b"\x1b[13;2u"), "shift+enter is a newline, not a send");
        // Moving around, mouse, focus and alt keys type nothing.
        for keys in [&b"\x1b[A\x1bOB"[..], b"\x1b[<0;10;5M", b"\x1b[M !!", b"\x1b[I", b"\x1bb", b"\x1b]11;?\x07"] {
            assert!(!d(keys), "{keys:?}");
        }
        assert!(drafting(true, b"\x1b[D"), "a draft stays while the cursor moves");
        // A pasted newline is part of the draft, not a send.
        assert!(d(b"\x1b[200~one\rtwo\x1b[201~"));
    }

    #[test]
    fn pastes_are_bracketed_only_when_asked_and_cannot_end_early() {
        assert_eq!(paste(true, "hi"), b"\x1b[200~hi\x1b[201~");
        assert_eq!(paste(false, "hi"), b"hi");
        assert_eq!(paste(true, "a\x1b[201~b"), b"\x1b[200~ab\x1b[201~");
    }
}
