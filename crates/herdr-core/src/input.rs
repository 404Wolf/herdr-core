//! Input from agents: one queue per terminal, so send, keys and interrupt
//! never interleave with each other, and never with a person typing.

use crate::holder_proto as proto;
use crate::rpc::RpcError;
use crate::runtime::{Core, State};
use crate::terminals::Terminal;
use schemars::JsonSchema;
use serde::Serialize;
use std::time::{Duration, Instant};
use tokio::sync::watch;

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

/// Whether the caller of a call is still there to hear how it went. A send
/// (or keys, or interrupt) that is still waiting for its turn, for a person
/// who is typing or for a draft when its caller goes away (its `hc` was
/// killed, its connection hung up) stops waiting and types nothing: nobody
/// is left to hear that it failed, and the caller will have given it up.
#[derive(Clone, Default)]
pub struct Gone(Option<watch::Receiver<bool>>);

impl Gone {
    /// A caller that is here until `ended` is told otherwise (or dropped).
    pub fn watch(ended: &watch::Sender<bool>) -> Gone {
        Gone(Some(ended.subscribe()))
    }

    pub fn is_gone(&self) -> bool {
        // A dropped sender ends the caller too.
        self.0.as_ref().is_some_and(|rx| *rx.borrow() || rx.has_changed().is_err())
    }

    /// Resolves once the caller is gone; never for a call with no caller to lose.
    pub async fn ended(&self) {
        match &self.0 {
            Some(rx) => drop(rx.clone().wait_for(|gone| *gone).await),
            None => std::future::pending().await,
        }
    }
}

/// The error of a call dropped because its caller went away (nobody hears it).
fn dropped(arrived: Instant) -> RpcError {
    RpcError::new(
        "cancelled",
        format!("its caller went away after {} s; nothing was typed", arrived.elapsed().as_secs()),
        "",
    )
}

/// A person's typing, as it affects agent input to one terminal.
#[derive(Default)]
pub struct Gate {
    last_human: Option<Instant>,
    /// A person's last real input here: typed, clicked or pasted (never a
    /// keepalive or a terminal's reply). A draft nobody has touched for
    /// `draft_idle` expires.
    last_typed: Option<Instant>,
    /// An agent's input is going in right now: viewer input waits.
    busy: bool,
    held: Vec<u8>,
    held_human: bool,
    /// What a person's keys have left in the program's input.
    typing: Typing,
    /// A person typed text they haven't sent or cleared yet (see `Typing`):
    /// an agent's text waits for it.
    draft: Option<Draft>,
}

/// A person's unsent draft: since when, and who typed into it last.
struct Draft {
    since: Instant,
    since_ms: u64,
    by: Option<DraftAuthor>,
    /// When `by` detached (None: still attached).
    left: Option<Instant>,
}

/// The viewer that typed into a draft last.
#[derive(Serialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct DraftAuthor {
    /// As in `hc viewers`: the connection and channel it typed on.
    pub connection_id: u64,
    pub channel: u32,
    pub origin: crate::viewers::ViewerOrigin,
    /// Its peer's name, when it typed through another machine.
    pub peer: Option<String>,
}

/// A person's draft that agents' text waits for (`hc get`, `hc viewers`).
#[derive(Serialize, JsonSchema, Clone, Debug, PartialEq, Eq)]
pub struct DraftView {
    /// When the person began typing it, ms since the epoch.
    pub since_ms: u64,
    /// The viewer that typed into it last; null if unknown.
    pub by: Option<DraftAuthor>,
    /// That viewer is still attached.
    pub attached: bool,
    /// The characters it holds, as the core followed the keys; null when it
    /// can't tell (anything but plain text and backspaces went in): then only
    /// sending or clearing it (Enter, ctrl+c, ctrl+u) ends it, or expiry.
    pub chars: Option<usize>,
    /// Ms since a person last typed on this terminal.
    pub input_ms_ago: u64,
    /// Ms until it expires and agents' text goes in again: after
    /// `HC_DRAFT_IDLE_MS` (10 min) with nobody typing on the terminal, or
    /// `HC_DRAFT_LEFT_MS` (1 min) after the viewer that typed it detached
    /// with no viewer left.
    pub expires_in_ms: u64,
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
            // What a person's last frame began (an Esc, half a character) went
            // to the program before the agent's bytes: it ends there.
            if !std::mem::take(&mut self.typing.pending).is_empty() {
                self.typing.lost();
            }
            Vec::new()
        } else {
            self.held_human = false;
            std::mem::take(&mut self.held)
        }
    }

    /// Viewer bytes: straight through, or held while an agent types.
    /// Only human input resets the quiet period.
    pub fn viewer(&mut self, bytes: &[u8], human: bool) -> Vec<u8> {
        self.input(bytes, human, human && !bytes.is_empty(), None)
    }

    /// [`Gate::viewer`] for a known viewer `by`. `person`: a person's real
    /// input (not empty, not a terminal's reply), which keeps a draft alive.
    pub fn input(&mut self, bytes: &[u8], human: bool, person: bool, by: Option<DraftAuthor>) -> Vec<u8> {
        let now = Instant::now();
        if human {
            self.last_human = Some(now);
            if !self.busy {
                self.typing.keys(bytes);
            } else if !bytes.is_empty() {
                // Held until the agent's input is in, then delivered after it:
                // the core doesn't follow such keys, it only knows the input
                // now holds something a person typed.
                self.typing.pending.clear();
                self.typing.opaque();
            }
            if !self.typing.drafting() {
                self.draft = None;
            } else if self.draft.is_none() {
                let by = by.clone();
                self.draft = Some(Draft { since: now, since_ms: crate::runtime::now_ms(), by, left: None });
            }
        }
        if person {
            self.last_typed = Some(now);
            if let Some(draft) = self.draft.as_mut().filter(|_| by.is_some()) {
                (draft.by, draft.left) = (by, None);
            }
        }
        if self.busy {
            self.held_human |= human && !bytes.is_empty();
            self.held.extend_from_slice(bytes);
            Vec::new()
        } else {
            bytes.to_vec()
        }
    }

    /// Whether a person's draft is waiting to be sent or cleared.
    pub fn drafting(&self) -> bool {
        self.draft.is_some()
    }

    /// The viewer `connection_id`/`channel` detached.
    pub fn left(&mut self, connection_id: u64, channel: u32) {
        if let Some(draft) = &mut self.draft
            && draft.by.as_ref().is_some_and(|b| (b.connection_id, b.channel) == (connection_id, channel))
        {
            draft.left.get_or_insert_with(Instant::now);
        }
    }

    /// An agent's input went in (`text`: a message, ended by its submit key).
    fn typed(&mut self, text: bool) {
        if self.held_human {
            // A person's keys go in after the agent's: what the core made of
            // them stands, but it can no longer vouch for the input.
            return self.typing.lost();
        }
        // What a person's last frame began (an Esc, half a character) went in
        // with the agent's bytes: it must not join their next key.
        let carry = !std::mem::take(&mut self.typing.pending).is_empty();
        if !text || carry || self.typing.drafting() {
            // Keys (moving the cursor, say), a sequence the agent's bytes
            // finished, or a person's draft the agent's input went into.
            self.typing.lost();
        } else {
            // Submitted from an input the core knew: it is empty again.
            self.typing.unsure = false;
        }
        if !self.typing.drafting() {
            self.draft = None;
        }
    }

    /// When a person last touched the draft's terminal.
    fn last_input(&self, draft: &Draft) -> Instant {
        self.last_typed.map_or(draft.since, |t| t.max(draft.since))
    }

    /// How long until the draft expires. `viewers_left`: an interactive viewer is still attached.
    fn expires_in(&self, draft: &Draft, viewers_left: bool) -> Duration {
        let idle = draft_idle().saturating_sub(self.last_input(draft).elapsed());
        match draft.left {
            Some(left) if !viewers_left => idle.min(draft_left_after().saturating_sub(left.elapsed())),
            _ => idle,
        }
    }

    /// Ends a draft nobody will send: nobody typed on the terminal for
    /// `draft_idle`, or the viewer that typed it detached `draft_left_after`
    /// ago and no viewer is left. Returns why, when it did. The core can no
    /// longer follow what the program's input holds after.
    pub fn expire(&mut self, viewers_left: bool) -> Option<String> {
        let draft = self.draft.as_ref()?;
        if !self.expires_in(draft, viewers_left).is_zero() {
            return None;
        }
        let idle = self.last_input(draft).elapsed();
        let why = if idle >= draft_idle() {
            format!("nobody typed on it for {} s", idle.as_secs())
        } else {
            let left = draft.left.map_or(0, |l| l.elapsed().as_secs());
            format!("the viewer that typed it detached {left} s ago and no viewer is left")
        };
        self.draft = None;
        self.typing = Typing { unsure: true, ..Typing::default() };
        Some(why)
    }

    /// The draft as clients see it. `attached`: whether a viewer is still attached.
    pub fn view(&self, attached: impl Fn(&DraftAuthor) -> bool, viewers_left: bool) -> Option<DraftView> {
        let draft = self.draft.as_ref()?;
        Some(DraftView {
            since_ms: draft.since_ms,
            by: draft.by.clone(),
            attached: draft.by.as_ref().is_some_and(attached),
            chars: self.typing.chars(),
            input_ms_ago: self.last_input(draft).elapsed().as_millis() as u64,
            expires_in_ms: self.expires_in(draft, viewers_left).as_millis() as u64,
        })
    }
}

/// A draft longer than this is not followed character by character.
const DRAFT_CHARS_MAX: usize = 100_000;
/// An escape sequence still open after this many bytes is not followed.
const PENDING_MAX: usize = 512;

/// What a person's keys have left in a program's input, as far as the core
/// can follow them. Only a provably empty input ends a draft early; anything
/// else keeps it (until Enter, ctrl+c or ctrl+u, or expiry).
///
/// Text starts a draft; Enter, ctrl+c or ctrl+u ends it (plain or as CSI u
/// keys). A draft ends early only on a whitelist: since it began, a person
/// sent nothing but printable text (UTF-8, or CSI u text keys: their text,
/// shifted form or a capital letter; their releases and lone modifier keys
/// too) and plain backspaces (DEL, ctrl+h, CSI u backspace), no agent input
/// went in, and the backspaces took back everything typed. Anything else
/// makes it uncertain: any other escape sequence (arrows, Esc, alt chords,
/// mouse, focus, a terminal's replies), a paste, word deletion, other
/// control keys, or an agent's send, keys or interrupt. Uncertainty before a
/// draft begins (an arrow recalling history, say) carries into it, except
/// what cannot put text in the input (mouse motion, wheel and release, focus,
/// replies), until the next Enter, ctrl+c or ctrl+u, or an agent's message
/// sent while the input was known. A character or escape sequence cut across
/// frames is put back together first; inside a paste only its end marker
/// counts. Keys a person presses while an agent's input goes in are held and
/// delivered after it: they are not followed, they make an uncertain draft
/// (see `Gate::input`), and what a frame began before the agent's input ends
/// there.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Typing {
    /// None: no draft. Some(None): a draft the core can't follow.
    /// Some(Some(text)): exactly what was typed.
    draft: Option<Option<Vec<char>>>,
    /// Since the last send or clear something happened the core can't follow.
    unsure: bool,
    /// Inside a bracketed paste: everything up to its end marker is content.
    paste: bool,
    /// The start of a character or escape sequence (or, in a paste, of its
    /// end marker) the last frame cut off.
    pending: Vec<u8>,
}

/// What ends a bracketed paste.
const PASTE_END: &[u8] = b"\x1b[201~";

impl Typing {
    /// A draft is there, or text is still arriving (a paste, a character cut in two).
    pub fn drafting(&self) -> bool {
        self.draft.is_some() || self.pending.first().is_some_and(|b| *b >= 0x80) || self.paste
    }

    /// The draft's length, when the core can tell.
    pub fn chars(&self) -> Option<usize> {
        if !self.pending.is_empty() || self.paste {
            return None;
        }
        self.draft.as_ref()?.as_ref().map(Vec::len)
    }

    /// The input is known to be empty again.
    fn clear(&mut self) {
        *self = Typing::default();
    }

    /// Something happened to the input the core can't follow.
    pub(crate) fn lost(&mut self) {
        self.unsure = true;
        if let Some(text) = &mut self.draft {
            *text = None;
        }
    }

    /// Something was put into the input, the core can't tell exactly what.
    fn opaque(&mut self) {
        self.unsure = true;
        self.draft = Some(None);
    }

    /// Something that can't put text in the input: it changes nothing before
    /// a draft, but isn't on the whitelist once one began.
    fn inert(&mut self) {
        if self.draft.is_some() {
            self.lost();
        }
    }

    fn text(&mut self, c: char) {
        match &mut self.draft {
            None => self.draft = Some((!self.unsure).then(|| vec![c])),
            Some(Some(text)) if text.len() < DRAFT_CHARS_MAX => text.push(c),
            Some(text) => *text = None,
        }
    }

    fn back(&mut self) {
        if let Some(Some(text)) = &mut self.draft {
            text.pop();
            if text.is_empty() {
                self.draft = None;
            }
        }
    }

    /// A person's keys, as one viewer sent them (one frame).
    pub fn keys(&mut self, frame: &[u8]) {
        let mut bytes = std::mem::take(&mut self.pending);
        bytes.extend_from_slice(frame);
        let mut i = 0;
        while i < bytes.len() {
            let rest = &bytes[i..];
            if self.paste {
                // Content up to the end marker, whatever it holds (another
                // start marker included); the draft is already uncertain.
                match rest.windows(PASTE_END.len()).position(|w| w == PASTE_END) {
                    Some(at) => {
                        self.paste = false;
                        i += at + PASTE_END.len();
                        continue;
                    }
                    None => {
                        let keep = (1..PASTE_END.len()).rev().find(|n| rest.ends_with(&PASTE_END[..*n])).unwrap_or(0);
                        self.pending = rest[rest.len() - keep..].to_vec();
                        return;
                    }
                }
            }
            if rest[0] == 0x1b {
                if escape_open(rest) {
                    if rest.len() <= PENDING_MAX {
                        self.pending = rest.to_vec();
                    } else {
                        self.lost();
                    }
                    return;
                }
                let (len, csi) = escape(rest);
                let len = match rest.get(1) {
                    Some(b'P' | b'_') => string_end(rest).unwrap_or(rest.len()),
                    _ => len,
                };
                let seq = &rest[..len];
                match csi {
                    Some((b"200", b'~')) => {
                        self.opaque();
                        self.paste = true;
                    }
                    _ if terminal_reply(seq) => self.inert(),
                    Some((params, b'u')) => self.csi_key(params),
                    Some((params, end @ (b'M' | b'm'))) if params.first() == Some(&b'<') => {
                        let button = std::str::from_utf8(&params[1..])
                            .ok()
                            .and_then(|p| p.split(';').next()?.parse::<u32>().ok())
                            .unwrap_or(0);
                        self.mouse(button, end == b'm');
                    }
                    None if rest.get(2) == Some(&b'M') && rest.get(1) == Some(&b'[') && len == 6 => {
                        let button = u32::from(rest[3].wrapping_sub(32));
                        self.mouse(button, button & 3 == 3);
                    }
                    None if matches!(rest.get(1), Some(b']' | b'P' | b'_')) => self.inert(),
                    _ => self.lost(),
                }
                i += len;
                continue;
            }
            if utf8_open(rest) {
                self.pending = rest.to_vec();
                return;
            }
            let (c, len) = next_char(rest);
            match rest[0] {
                b'\r' | 0x03 | 0x15 => self.clear(),
                0x7f | 0x08 => self.back(),
                0x20.. => self.text(c),
                _ => self.lost(),
            }
            i += len;
        }
    }

    /// Motion, wheel and release can't put text in; a click may move the cursor.
    fn mouse(&mut self, button: u32, release: bool) {
        if !release && button & (32 | 64) == 0 { self.lost() } else { self.inert() }
    }

    /// A key in the CSI u (kitty) encoding:
    /// `code[:shifted[:base]][;modifiers[:event]][;text[:text...]]`.
    fn csi_key(&mut self, params: &[u8]) {
        let Ok(params) = std::str::from_utf8(params) else { return self.lost() };
        let mut fields = params.split(';');
        let mut codes = fields.next().unwrap_or("").split(':');
        let key: Option<u32> = codes.next().and_then(|k| k.parse().ok());
        let shifted: Option<u32> = codes.next().filter(|s| !s.is_empty()).and_then(|s| s.parse().ok());
        let mut mods = fields.next().unwrap_or("1").split(':');
        let (mods, event): (Option<u32>, _) =
            (mods.next().filter(|m| !m.is_empty()).map_or(Some(1), |m| m.parse().ok()), mods.next());
        if event == Some("3") {
            return; // a key let go: its press said what it did
        }
        let (Some(key), Some(mods)) = (key, mods) else { return self.lost() };
        // Caps and num lock change nothing typed here.
        let mods: u32 = (mods.saturating_sub(1) & !(64 | 128)) + 1;
        // The text the key typed, when the terminal says.
        if let Some(text) = fields.next().filter(|t| !t.is_empty()) {
            let chars: Option<Vec<char>> = text.split(':').map(|c| c.parse().ok().and_then(char::from_u32)).collect();
            match chars {
                Some(chars) if !chars.is_empty() && chars.iter().all(|c| !c.is_control()) => {
                    chars.into_iter().for_each(|c| self.text(c))
                }
                _ => self.opaque(),
            }
            return;
        }
        let printable = |c: u32| c >= 32 && c != 127 && !(57344..=63743).contains(&c) && char::from_u32(c).is_some();
        match (key, mods) {
            (13, 1) | (99 | 117, 5) => self.clear(),
            (127 | 8, 1) | (104, 5) => self.back(),
            // A modifier key alone.
            (57441..=57452, _) => {}
            (c, 1) if printable(c) => self.text(char::from_u32(c).expect("printable")),
            (c, 2) if printable(c) => match shifted {
                Some(s) if printable(s) => self.text(char::from_u32(s).expect("printable")),
                None if (97..=122).contains(&c) => self.text(char::from_u32(c - 32).expect("ascii")),
                // It typed something, the core can't tell what.
                _ => self.opaque(),
            },
            // shift+enter: a newline in the draft, for some programs.
            (13, 2) => self.opaque(),
            _ => self.lost(),
        }
    }
}

/// Whether the escape sequence `bytes` starts with is cut off at its end
/// (a lone ESC too: the next frame says whether it was the Esc key).
fn escape_open(bytes: &[u8]) -> bool {
    match bytes.get(1) {
        None => true,
        Some(b'[') if bytes.get(2) == Some(&b'M') => bytes.len() < 6,
        Some(b'[') => !bytes[2..].iter().any(|b| (0x40..=0x7e).contains(b)),
        Some(b']' | b'P' | b'_') => string_end(bytes).is_none(),
        Some(b'O') => bytes.len() < 3,
        Some(_) => false,
    }
}

/// Whether `bytes` starts with a UTF-8 character cut off at its end.
fn utf8_open(bytes: &[u8]) -> bool {
    let len = match bytes[0] {
        0xf0..=0xf7 => 4,
        0xe0..=0xef => 3,
        0xc2..=0xdf => 2,
        _ => return false,
    };
    bytes.len() < len && bytes[1..].iter().all(|b| b & 0xc0 == 0x80)
}

/// The character `bytes` starts with (UTF-8), and its length; a byte that
/// starts none is one replacement character.
fn next_char(bytes: &[u8]) -> (char, usize) {
    let len = match bytes[0] {
        0xf0.. => 4,
        0xe0.. => 3,
        0xc0.. => 2,
        _ => 1,
    };
    match bytes.get(..len).map(std::str::from_utf8) {
        Some(Ok(s)) => (s.chars().next().unwrap_or('\u{fffd}'), len),
        _ => ('\u{fffd}', 1),
    }
}

/// Whether a person's keys leave a draft in the program's input (see
/// [`Typing`]), from a draft the core can't follow (`draft`) or none.
pub fn drafting(draft: bool, bytes: &[u8]) -> bool {
    let mut typing = Typing { draft: draft.then_some(None), unsure: draft, ..Typing::default() };
    typing.keys(bytes);
    typing.drafting()
}

/// Whether `bytes` is only a terminal's answers to a program's queries, sent
/// as DATA by a path without AUTOMATIC_INPUT: device attributes
/// (`CSI ? .. c`, `CSI > .. c`), status and cursor reports (`CSI .. n`,
/// `CSI r ; c R`, `CSI ? r ; c R`), mode reports (`CSI .. $ y`), window
/// reports (`CSI 4|6|8|48 ; .. t`), keyboard flags (`CSI ? .. u`), focus
/// reports (`CSI I`, `CSI O`), and OSC, DCS and APC strings. No key a person
/// presses is one of these, except a cursor report shaped like a modified F3
/// (`CSI 1 ; 5 R`): that key alone then does not count as use. Empty: no.
pub fn terminal_reply(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            return false;
        }
        let rest = &bytes[i..];
        let reply = match rest.get(1) {
            Some(b'[') => match escape(rest) {
                (_, Some((params, final_byte))) => csi_reply(params, final_byte),
                _ => false,
            },
            Some(b']' | b'P' | b'_') => string_end(rest).is_some(),
            _ => false,
        };
        if !reply {
            return false;
        }
        i += match rest.get(1) {
            Some(b'[') => escape(rest).0,
            _ => string_end(rest).unwrap_or(rest.len()),
        };
    }
    !bytes.is_empty()
}

/// Classifies one viewer's DATA frames as terminal replies (see
/// [`terminal_reply`]), across frame boundaries: a reply a path cut in two
/// (`ESC [ 2 4 ;` then `8 0 R`) is still a reply. An escape sequence still
/// open at a frame's end, even a lone ESC, is kept back and judged with the
/// next frame. So an Escape key pressed alone counts as use only with the
/// next key: a reply cut right after its ESC must never count.
#[derive(Default, Debug, Clone)]
pub struct Replies {
    carry: Vec<u8>,
    /// A resumed stream (after a dropped line): what came so far that may
    /// still be the tail of a reply whose start went out on the old stream,
    /// and when its first byte came. None: not resumed, or settled.
    tail: Option<(Vec<u8>, Option<std::time::Instant>)>,
}

/// Starts a reply could have had, to judge a resumed stream's first frames
/// as its tail ("" too: the tail may begin with a whole reply).
const REPLY_STARTS: &[&[u8]] = &[
    b"",
    b"\x1b",
    b"\x1b[",
    b"\x1b[1;",
    b"\x1b[1;1",
    b"\x1b[?",
    b"\x1b[?1;",
    b"\x1b[8;",
    b"\x1b]",
    b"\x1b]\x1b",
    b"\x1bP",
    b"\x1b_",
];

/// A possible reply tail is given up (and counts as use) past this...
const TAIL_MAX: usize = 64;
/// ...or once its first byte is this old.
const TAIL_TIME: std::time::Duration = std::time::Duration::from_secs(1);

enum Tail {
    /// Finishes a reply (and any whole replies after it).
    Done,
    /// Can still finish one: more is to come.
    Open,
    /// Cannot: a person.
    Not,
}

impl Replies {
    /// For a resumed stream (after a dropped line): see `tail`.
    pub fn resumed() -> Self {
        Replies { carry: Vec::new(), tail: Some((Vec::new(), None)) }
    }

    /// Whether this frame is only terminal replies (or the start of one, kept
    /// back until it ends). Empty: no.
    ///
    /// On a resumed stream, the frames up to the first that settles it are
    /// judged together as the possible tail of a reply cut by the reconnect
    /// (`ESC[24;` before the drop; `8`, `0R` after): not use while every byte
    /// can still finish a reply, settled as a reply when one is finished, and
    /// use the moment a byte proves it cannot be one. Bounded: past
    /// [`TAIL_MAX`] bytes or [`TAIL_TIME`] the frame is judged on its own. A
    /// person's first keys after a reconnect that could still be a reply's
    /// tail (visible characters with no space, Enter or control key) count
    /// once the bound or such a byte decides.
    pub fn only_replies(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return false;
        }
        if let Some((tail, since)) = &mut self.tail {
            tail.extend_from_slice(bytes);
            let since = *since.get_or_insert_with(std::time::Instant::now);
            let state = if tail.len() > TAIL_MAX || since.elapsed() > TAIL_TIME { Tail::Not } else { reply_tail(tail) };
            match state {
                Tail::Open => return true,
                Tail::Done => {
                    self.tail = None;
                    return true;
                }
                Tail::Not => self.tail = None,
            }
        }
        let mut all = std::mem::take(&mut self.carry);
        all.extend_from_slice(bytes);
        let open = open_tail(&all);
        if let Some(at) = open
            && all.len() - at <= MAX_REPLY
        {
            self.carry = all.split_off(at);
        }
        if all.is_empty() {
            return true;
        }
        terminal_reply(&all)
    }
}

/// Whether `tail` can be what follows some start of a reply: whole replies,
/// ending in one still open that can still finish (see [`viable`]).
fn reply_tail(tail: &[u8]) -> Tail {
    let mut open = false;
    for start in REPLY_STARTS {
        let all = [*start, tail].concat();
        let (whole, rest) = match open_tail(&all) {
            Some(at) => all.split_at(at),
            None => (&all[..], &all[..0]),
        };
        if !whole.is_empty() && !terminal_reply(whole) {
            continue;
        }
        if rest.is_empty() {
            return Tail::Done;
        }
        open |= viable(rest);
    }
    if open { Tail::Open } else { Tail::Not }
}

/// An open sequence that can still become a reply: a lone ESC; a CSI with
/// only parameter bytes so far; an OSC, DCS or APC with only visible bytes
/// so far, no space (and maybe the ESC of its terminator).
fn viable(open: &[u8]) -> bool {
    match open.get(1) {
        None => open == b"\x1b",
        Some(b'[') => open[2..].iter().all(|b| b.is_ascii_digit() || matches!(b, b';' | b'?' | b'>' | b'=' | b'$')),
        Some(b']' | b'P' | b'_') => {
            let body = open[2..].strip_suffix(b"\x1b").unwrap_or(&open[2..]);
            body.iter().all(|b| (0x21..0x7f).contains(b))
        }
        _ => false,
    }
}

/// Longer than this, an unfinished sequence is not a reply being cut.
const MAX_REPLY: usize = 512;

/// Where an escape sequence still open at the end of `bytes` begins.
fn open_tail(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    let mut open = None;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        let rest = &bytes[i..];
        let len = match rest.get(1) {
            Some(b'[') => {
                let end = rest[2..].iter().position(|b| (0x40..=0x7e).contains(b));
                end.map(|n| n + 3)
            }
            Some(b']' | b'P' | b'_') => string_end(rest),
            Some(_) => Some(2),
            None => None,
        };
        match len {
            Some(n) => {
                i += n;
                open = None;
            }
            None => {
                open = Some(i);
                break;
            }
        }
    }
    open
}

/// A CSI only terminals send back: see [`terminal_reply`].
fn csi_reply(params: &[u8], final_byte: u8) -> bool {
    let digits = |p: &[u8]| !p.is_empty() && p.iter().all(|b| b.is_ascii_digit() || *b == b';');
    match final_byte {
        b'c' => matches!(params.first(), Some(b'?' | b'>' | b'=')) && params.len() > 1,
        b'n' => digits(params),
        b'R' => {
            let p = params.strip_prefix(b"?").unwrap_or(params);
            digits(p) && p.contains(&b';')
        }
        b'y' => {
            params.ends_with(b"$")
                && digits(params.strip_prefix(b"?").unwrap_or(params).strip_suffix(b"$").unwrap_or(b""))
        }
        b't' => digits(params) && [&b"4;"[..], b"6;", b"8;", b"48;"].iter().any(|p| params.starts_with(p)),
        b'u' => params.first() == Some(&b'?') && (params.len() == 1 || digits(&params[1..])),
        b'I' | b'O' => params.is_empty(),
        _ => false,
    }
}

/// The length of a complete OSC, DCS or APC string at the start of `bytes`
/// (ended by BEL, or ST: `ESC \`); None: unterminated.
fn string_end(bytes: &[u8]) -> Option<usize> {
    let body = &bytes[2..];
    body.iter().enumerate().find_map(|(n, b)| match b {
        0x07 if bytes[1] == b']' => Some(n + 3),
        0x1b if body.get(n + 1) == Some(&b'\\') => Some(n + 4),
        _ => None,
    })
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

/// How long an agent's text waits for a person's draft to be sent or cleared.
fn draft_max() -> Duration {
    env_ms("HC_DRAFT_MAX_MS", 120_000)
}

/// A draft nobody typed on its terminal for this long expires (`HC_DRAFT_IDLE_MS`).
pub fn draft_idle() -> Duration {
    env_ms("HC_DRAFT_IDLE_MS", 600_000)
}

/// A draft whose viewer detached this long ago, with no viewer left, expires (`HC_DRAFT_LEFT_MS`).
pub fn draft_left_after() -> Duration {
    env_ms("HC_DRAFT_LEFT_MS", 60_000)
}

/// A send refused because a person's draft is still there: what it is, and how it ends.
fn drafting_error(id: u32, draft: &DraftView) -> RpcError {
    let t = crate::terminals::address(id);
    let ago = |ms: u64| match ms / 1000 {
        s if s < 120 => format!("{s} s"),
        s => format!("{} min", s / 60),
    };
    let since = ago(crate::runtime::now_ms().saturating_sub(draft.since_ms));
    let by = match &draft.by {
        Some(by) => format!(
            " by viewer c{}/ch{}{}",
            by.connection_id,
            by.channel,
            if draft.attached { "" } else { " (detached)" }
        ),
        None => String::new(),
    };
    RpcError::new(
        "drafting",
        format!(
            "{t} has a draft a person hasn't sent (typed{by} {since} ago, last input {} ago)",
            ago(draft.input_ms_ago)
        ),
        format!(
            "nothing was typed; it ends when they send or clear it (Enter, ctrl+c, ctrl+u, or backspacing it away), \
             or by itself in {} with nobody typing there; `hc get {t}` shows it",
            ago(draft.expires_in_ms)
        ),
    )
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
    /// Queued earlier under the same key: not queued again.
    pub repeated: bool,
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
    /// for them to send or clear it, and refuses after `draft_max`, counted
    /// from when the call arrived (not from its turn: calls queued behind one
    /// draft give up together, not one every `draft_max`).
    /// `gone`: its caller leaving ends the wait with nothing typed; once the
    /// typing has begun it is finished (never half a paste).
    pub async fn type_in<T>(
        &self,
        id: u32,
        quiet: Duration,
        text: bool,
        gone: &Gone,
        prepare: impl FnOnce(&State, &Terminal) -> Result<(T, Vec<Step>), RpcError>,
    ) -> Result<T, RpcError> {
        let arrived = Instant::now();
        let give_up = arrived + draft_max();
        let queue = self.lock().queues.entry(id).or_default().clone();
        let _turn = tokio::select! {
            turn = queue.lock() => turn,
            () = gone.ended() => return Err(dropped(arrived)),
        };
        loop {
            let (left, draft) = {
                let mut st = self.lock();
                let draft = if text { crate::viewers::draft(&mut st, id) } else { None };
                let gate = st.gates.get(&id);
                let left = gate.and_then(|g| g.last_human).map(|t| quiet.saturating_sub(t.elapsed()));
                (left, draft)
            };
            let nap = match (left, draft) {
                (Some(left), _) if !left.is_zero() => left,
                (_, Some(_)) if Instant::now() < give_up => DRAFT_POLL,
                (_, Some(draft)) => return Err(drafting_error(id, &draft)),
                _ => break,
            };
            tokio::select! {
                () = tokio::time::sleep(nap) => {}
                () = gone.ended() => return Err(dropped(arrived)),
            }
        }
        if gone.is_gone() {
            return Err(dropped(arrived));
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
        gate.typed(text);
        let held = gate.set_busy(false);
        if let Some(link) = st.link(id) {
            link.input(id, &held);
        }
        Ok(out)
    }

    /// Paste + Enter, after a person has paused typing. Refuses a blocked
    /// program (typing would answer its dialog) unless `force`.
    pub async fn send(
        &self,
        id: u32,
        text: &str,
        force: bool,
        only_if: Option<&str>,
        gone: &Gone,
    ) -> Result<Sent, RpcError> {
        self.send_message(id, text, force, only_if, false, gone).await
    }

    /// The task a new terminal was made for: typed whether or not its starter stays.
    pub(crate) async fn send_first_task(&self, id: u32, text: &str) -> Result<Sent, RpcError> {
        self.send_message(id, text, false, None, true, &Gone::default()).await
    }

    async fn send_message(
        &self,
        id: u32,
        text: &str,
        force: bool,
        only_if: Option<&str>,
        first_task: bool,
        gone: &Gone,
    ) -> Result<Sent, RpcError> {
        if let Some(t) = self.lock().reg.terminals.get(&id) {
            t.check_program()?;
        }
        self.settle(id, gone).await?;
        let (status, turn, reported) = self
            .type_in(id, quiet(), true, gone, |st, t| {
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
        Ok(Sent { status, turn, started, via: "typed", waiting: None, repeated: false })
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
    async fn settle(&self, id: u32, gone: &Gone) -> Result<(), RpcError> {
        let started = Instant::now();
        let deadline = started + SETTLE_MAX;
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
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
                () = gone.ended() => return Err(dropped(started)),
            }
        }
    }

    /// Named keys, after a person has paused typing.
    pub async fn keys(&self, id: u32, names: &[String], gone: &Gone) -> Result<(Option<String>, u64), RpcError> {
        self.type_in(id, quiet(), false, gone, |st, t| {
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
    pub async fn interrupt(&self, id: u32, timeout: Duration, gone: &Gone) -> Result<(Interrupted, String), RpcError> {
        let (reported, pid) = {
            let st = self.lock();
            let t = st.reg.terminals.get(&id).ok_or_else(|| RpcError::new("not_found", "terminal removed", ""))?;
            t.check_program()?;
            (t.reported, t.pid.unwrap_or(0) as i32)
        };
        let before = if reported { -1 } else { self.foreground(id).await };
        let key = self
            .type_in(id, Duration::ZERO, false, gone, |st, t| {
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
    fn a_resumed_stream_judges_a_reply_tail_across_frames() {
        // Tails of a CPR, DA, DECRPM, window report and OSC, whole or cut
        // again after the reconnect: never use.
        for frames in [
            &[&b"80R"[..]][..],
            &[b"24;80R"],
            &[b"8", b"0R"],
            &[b"4;", b"8", b"0R"],
            &[b"[?62;22c"],
            &[b"62", b";22", b"c"],
            &[b"2004;1$y"],
            &[b"24;", b"80t"],
            &[b"11;rgb:00", b"00/0000/0000\x1b", b"\\"],
            &[b"0000/0000\x07"],
            &[b"\\"],
            &[b"80R\x1b[?62c"],
        ] {
            let mut r = Replies::resumed();
            for frame in frames {
                assert!(r.only_replies(frame), "{frames:?}");
            }
            assert!(!r.only_replies(b"x"), "settled: the next key is a person ({frames:?})");
        }
        // A person's keys count within the frame that proves it.
        for key in [&b"ls\r"[..], b"\x1b[A", b"hello world", b"\r", b"\x03"] {
            assert!(!Replies::resumed().only_replies(key), "{:?}", String::from_utf8_lossy(key));
        }
        let mut r = Replies::resumed();
        assert!(r.only_replies(b"12"), "could still be a reply's tail");
        assert!(!r.only_replies(b" "), "no reply goes on so: a person");
        // Bounded: past TAIL_MAX bytes it is judged on its own.
        let mut r = Replies::resumed();
        assert!(r.only_replies(&[b'a'; TAIL_MAX]), "an OSC body could still be this long");
        assert!(!r.only_replies(b"b"), "past the bound: a person");
        // Only until settled: later frames are judged as usual.
        let mut r = Replies::resumed();
        assert!(!r.only_replies(b"ls\r"));
        assert!(!r.only_replies(b"80R"));
    }

    #[test]
    fn replies_cut_across_frames_are_still_replies() {
        let mut r = Replies::default();
        assert!(r.only_replies(b"\x1b[24;"), "an open reply is held back, not use");
        assert!(r.only_replies(b"80R"));
        assert!(r.only_replies(b"\x1b]11;rgb:0000/"));
        assert!(r.only_replies(b"0000/0000\x1b"));
        assert!(r.only_replies(b"\\"));
        assert!(r.only_replies(b"\x1b"), "a lone ESC is held back: a reply may follow");
        assert!(r.only_replies(b"[?62;22c"), "and it did");
        assert!(r.only_replies(b"\x1b"));
        assert!(!r.only_replies(b"x"), "ESC then a key is a person (alt+x)");
        assert!(r.only_replies(b"\x1b[1"));
        assert!(!r.only_replies(b"A"), "an arrow key cut in two is a key");
        assert!(!r.only_replies(b"ab\x1b[2"), "text before an open sequence is a person");
        assert!(!r.only_replies(b""));
    }

    #[test]
    fn terminal_replies_are_told_from_what_a_person_types() {
        for reply in [
            &b"\x1b[?62;22c"[..],
            b"\x1b[>1;10;0c",
            b"\x1b[0n",
            b"\x1b[24;80R",
            b"\x1b[?24;80;1R",
            b"\x1b[?2004;1$y",
            b"\x1b[8;24;80t",
            b"\x1b[4;480;640t",
            b"\x1b[48;24;80;480;640t",
            b"\x1b[?1u",
            b"\x1b[I",
            b"\x1b[O",
            b"\x1b]11;rgb:0000/0000/0000\x07",
            b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\",
            b"\x1bP1+r544e=787465726d\x1b\\",
            b"\x1b_Gi=1;OK\x1b\\",
            b"\x1b[24;80R\x1b[?62c",
        ] {
            assert!(terminal_reply(reply), "{:?}", String::from_utf8_lossy(reply));
        }
        for typed in [
            &b""[..],
            b"x",
            b"\r",
            b"\x1b",
            b"\x1b[A",
            b"\x1b[1;5A",
            b"\x1b[3~",
            b"\x1b[200~hi\x1b[201~",
            b"\x1b[<0;10;5M",
            b"\x1b[<64;10;5M",
            b"\x1b[97;5u",
            b"\x1bOP",
            b"\x1b[15~",
            b"\x1b[24;80Rx",
            b"\x1b]11;?",
        ] {
            assert!(!terminal_reply(typed), "{:?}", String::from_utf8_lossy(typed));
        }
    }

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

    fn author() -> DraftAuthor {
        DraftAuthor { connection_id: 7, channel: 1, origin: crate::viewers::ViewerOrigin::LocalDesktop, peer: None }
    }

    /// `age` ago, for the clock a gate reads.
    fn ago(age: Duration) -> Instant {
        Instant::now().checked_sub(age).expect("the clock is far enough along")
    }

    #[test]
    fn typed_then_backspaced_clears() {
        let d = |bytes: &[u8]| drafting(false, bytes);
        assert!(!d(b"hi\x7f\x7f"), "backspace");
        assert!(!d(b"hi\x08\x08"), "ctrl+h");
        assert!(!d(b"hi\x1b[127u\x1b[127u") && !d(b"hi\x1b[104;5u\x1b[104;5u"), "as CSI u keys");
        assert!(!d("é😀\x7f\x7f".as_bytes()), "a character is one backspace, however many bytes");
        assert!(d(b"hi\x7f"), "not all of it: still a draft");
        // Before a draft, what can't put text in (mouse motion, wheel and release, focus,
        // a terminal's reply) changes nothing; a let-go text key is part of typing it.
        assert!(!d(
            b"\x1b[<64;3;3M\x1b[<35;3;3M\x1b[<0;3;3m\x1b[I\x1b[O\x1b[24;80R\x1b]11;?\x07hi\x1b[105;1:3u\x7f\x7f"
        ));
        // Kitty keys with num lock on, shift alone, a shifted key (its shifted form
        // given, or a plain letter), and text the terminal names.
        assert!(!d(b"\x1b[57441;2u\x1b[104:72;130u\x1b[105;129u\x1b[127;129u\x1b[127;129u"));
        assert!(!d(b"\x1b[104;2u\x7f") && !d(b"\x1b[97;1;97:98u\x7f\x7f"));
        assert!(d(b"\x1b[97;1;97:98u\x7f"), "two characters of text, one taken back");
        // Cut across frames: a character, a CSI u backspace.
        let mut typing = Typing::default();
        for frame in [&[0xc3][..], &[0xa9], b"x\x1b[127;", b"1u"] {
            typing.keys(frame);
            assert!(typing.drafting(), "after {frame:?}");
        }
        typing.keys(b"\x7f");
        assert!(!typing.drafting(), "{typing:?}");
        // Across frames, through the gate, as a viewer sends them.
        let mut gate = Gate::default();
        gate.input(b"h", true, true, Some(author()));
        gate.input(b"i", true, true, Some(author()));
        assert!(gate.drafting());
        assert_eq!(gate.view(|_| true, true).unwrap().chars, Some(2));
        gate.input(b"\x7f", true, true, Some(author()));
        assert!(gate.drafting());
        gate.input(b"\x7f", true, true, Some(author()));
        assert!(!gate.drafting(), "the input is empty again");
        assert!(gate.view(|_| true, true).is_none());
        // Backspacing an empty input starts nothing.
        assert!(!d(b"\x7f\x7f\x17"));
    }

    #[test]
    fn typed_then_left_arrow_edit_stays() {
        let d = |bytes: &[u8]| drafting(false, bytes);
        // The cursor moved: the core can't tell what a backspace takes back.
        for edit in [
            &b"\x1b[D"[..],
            b"\x1b[1;5D",
            b"\x1bOD",
            b"\x1b[H",
            b"\x1b[3~",
            b"\x01",
            b"\t",
            b"\x1bb",
            b"\x1b[<0;3;3M",
            b"\x1b[27u",
            b"\x1b[97;5u",
            // Once a draft began, nothing off the whitelist: not even mouse
            // motion, wheel, focus or a terminal's reply.
            b"\x1b[<64;3;3M",
            b"\x1b[<35;3;3M",
            b"\x1b[I",
            b"\x1b[24;80R",
            b"\x1b]11;?\x07",
        ] {
            let mut keys = b"hi".to_vec();
            keys.extend_from_slice(edit);
            keys.extend_from_slice(b"\x7f\x7f\x7f\x7f");
            assert!(d(&keys), "unsure after {:?}", String::from_utf8_lossy(edit));
        }
        // Word deletion: programs disagree on what a word is (readline's alt+backspace
        // leaves `two_` of `two_words2`), so it is never counted down to empty.
        for keys in
            [&b"word\x17"[..], b"two_words2\x1b\x7f", b"x\x1b[119;5u", b"x\x1b[127;3u", b"one \x17\x7f\x7f\x7f\x7f"]
        {
            assert!(d(keys), "{:?}", String::from_utf8_lossy(keys));
        }
        // CSI u text that doesn't say what it typed is uncertain too.
        assert!(d(b"ab\x1b[49:33;2;33ucd\x1b\x7f"), "ab!cd, then a word taken back");
        assert!(d(b"\x1b[49;2u\x7f\x7f") && d(b"\x1b[13;2u\x7f\x7f") && d(b"\x1b[97;1;9u\x7f"));
        // Control bytes in a paste, even in a later frame, are pasted, never editing keys.
        let mut typing = Typing::default();
        typing.keys(b"\x1b[200~abc");
        typing.keys(b"\x7f\x7f\x7f\x1b[201~");
        typing.keys(b"\x7f\x7f\x7f\x7f\x7f\x7f");
        assert!(typing.drafting() && typing.chars().is_none(), "{typing:?}");
        // Any paste makes the draft uncertain; inside one only its end marker counts
        // (another start marker is content: readline inserts it).
        assert!(d(b"\x1b[200~ab\x1b[201~\x7f\x7f") && d(b"hi\x1b[200~\x1b[201~\x7f\x7f"));
        assert!(d(b"\x1b[200~a\x1b[Ab\x1b[201~\x7f\x7f"));
        let mut typing = Typing::default();
        typing.keys(b"\x1b[200~abc\x1b[200~\x1b[20");
        typing.keys(b"1~\x7f\x7f\x7f");
        assert!(typing.drafting() && !typing.paste, "{typing:?}");
        typing.keys(b"\r");
        assert!(!typing.drafting(), "Enter after the paste still clears it");
        // A key the core couldn't follow before the draft began (history recalled with
        // the up arrow, say) leaves the input unknown: backspacing what came after
        // doesn't end it.
        assert!(d(b"\x1b[Ahi\x7f\x7f"));
        let mut gate = Gate::default();
        gate.viewer(b"hi\x1b[D", true);
        gate.viewer(b"\x7f\x7f", true);
        assert!(gate.drafting());
        assert_eq!(gate.view(|_| true, true).unwrap().chars, None, "the length is unknown");
        // Esc pressed on its own (Esc twice clears some programs' input, or not).
        let mut gate = Gate::default();
        for frame in [&b"hi"[..], b"\x1b", b"\x7f\x7f"] {
            gate.viewer(frame, true);
        }
        assert!(gate.drafting());
    }

    #[test]
    fn enter_ctrl_c_ctrl_u_still_clear() {
        for clear in [&b"\r"[..], b"\x03", b"\x15", b"\x1b[13u", b"\x1b[99;5u", b"\x1b[117;5u"] {
            let mut gate = Gate::default();
            gate.viewer(b"hi\x1b[D", true);
            assert!(gate.drafting());
            gate.viewer(clear, true);
            assert!(!gate.drafting(), "{clear:?}");
            // Cleared, the input is known to be empty again: backspacing a new draft ends it.
            gate.viewer(b"x\x7f", true);
            assert!(!gate.drafting(), "followed again after {clear:?}");
        }
        // A message an agent sent (with its submit key) empties it as well.
        let mut gate = Gate::default();
        gate.viewer(b"\x1b[A", true);
        gate.typed(true);
        gate.viewer(b"x\x7f", true);
        assert!(!gate.drafting());
        // Keys an agent pressed did who knows what.
        gate.typed(false);
        gate.viewer(b"x\x7f", true);
        assert!(gate.drafting());
        // Even to a person's draft: `abc`, an agent's left arrow, three backspaces leave `c`.
        let mut gate = Gate::default();
        gate.viewer(b"abc", true);
        gate.typed(false);
        gate.viewer(b"\x7f\x7f\x7f", true);
        assert!(gate.drafting() && gate.view(|_| true, true).unwrap().chars.is_none());
        // A message sent while a person's draft is there doesn't vouch for it.
        let mut gate = Gate::default();
        gate.viewer(b"\x1b[Dab", true);
        gate.typed(true);
        gate.viewer(b"\x7f\x7f", true);
        assert!(gate.drafting());
        // An Esc a person's frame ended with went in with the agent's message: it
        // must not swallow their next key (`x` then backspace is not provably empty).
        let mut gate = Gate::default();
        gate.viewer(b"\x1b", true);
        assert!(!gate.drafting());
        gate.typed(true);
        gate.viewer(b"x", true);
        assert!(gate.drafting() && gate.view(|_| true, true).unwrap().chars.is_none());
        gate.viewer(b"\x7f", true);
        assert!(gate.drafting());
        // Keys held while an agent's input goes in make an uncertain draft, full stop:
        // an Esc before it doesn't swallow a held `x`, and plain held text (even
        // backspaced, even Enter) isn't followed.
        for held in [&b"x"[..], b"xy\x7f\x7f", b"\x1b", b"\r", b"\x1b[24;80R"] {
            let mut gate = Gate::default();
            gate.viewer(b"\x1b", true);
            gate.set_busy(true);
            assert!(gate.viewer(held, true).is_empty(), "held");
            gate.typed(true);
            assert_eq!(gate.set_busy(false), held);
            assert!(gate.drafting(), "{held:?}");
            assert!(gate.view(|_| true, true).unwrap().chars.is_none(), "{held:?}");
            gate.viewer(b"\x7f\x7f\x7f", true);
            assert!(gate.drafting(), "{held:?}");
            gate.viewer(b"\x15", true);
            assert!(!gate.drafting(), "ctrl+u still clears it");
        }
        // Nothing held: the agent's message leaves no draft.
        let mut gate = Gate::default();
        gate.set_busy(true);
        gate.typed(true);
        gate.set_busy(false);
        assert!(!gate.drafting());
        // Half a character too.
        let mut gate = Gate::default();
        gate.viewer(&[0xc3], true);
        gate.typed(false);
        gate.viewer(b"\xa9\x7f\x7f", true);
        assert!(gate.drafting());
    }

    #[test]
    fn inactivity_expiry_clears_after_n() {
        let mut gate = Gate::default();
        gate.input(b"half a thought", true, true, Some(author()));
        assert!(gate.expire(true).is_none(), "just typed");
        let view = gate.view(|_| true, true).unwrap();
        assert_eq!((view.by.clone(), view.attached, view.chars), (Some(author()), true, Some(14)));
        assert!(view.expires_in_ms > draft_idle().as_millis() as u64 - 5_000, "{view:?}");
        // Keepalives and terminal replies are not a person: they don't keep it alive.
        gate.last_typed = Some(ago(draft_idle() + Duration::from_secs(1)));
        gate.draft.as_mut().unwrap().since = ago(draft_idle() + Duration::from_secs(1));
        gate.input(b"", true, false, Some(author()));
        gate.input(b"\x1b[24;80R", true, false, Some(author()));
        assert!(gate.drafting());
        let why = gate.expire(true).expect("expired");
        assert!(why.contains("nobody typed on it for 601 s"), "{why}");
        assert!(!gate.drafting() && gate.view(|_| true, true).is_none());
        // What the input holds now is unknown: a new draft lasts until sent or cleared.
        gate.viewer(b"x\x7f", true);
        assert!(gate.drafting());
        // A person's input on the terminal keeps a draft alive.
        let mut gate = Gate::default();
        gate.input(b"hi", true, true, Some(author()));
        gate.draft.as_mut().unwrap().since = ago(draft_idle() + Duration::from_secs(1));
        gate.input(b"\x1b[A", true, true, Some(author()));
        assert!(gate.expire(true).is_none());
    }

    #[test]
    fn detach_of_the_only_drafting_viewer_clears_after_expiry() {
        let mut gate = Gate::default();
        gate.input(b"hi", true, true, Some(author()));
        gate.left(8, 1);
        assert!(gate.draft.as_ref().unwrap().left.is_none(), "another viewer left");
        gate.left(7, 1);
        assert!(gate.expire(false).is_none(), "not yet: it may come right back");
        gate.draft.as_mut().unwrap().left = Some(ago(draft_left_after() + Duration::from_secs(1)));
        assert!(gate.expire(true).is_none(), "another viewer is still attached: only inactivity ends it");
        let view = gate.view(|_| false, false).unwrap();
        assert!(!view.attached && view.expires_in_ms == 0, "{view:?}");
        let why = gate.expire(false).expect("expired");
        assert!(why.contains("detached 61 s ago and no viewer is left"), "{why}");
        // Typing again from a viewer: it is theirs now, and attached.
        let mut gate = Gate::default();
        gate.input(b"hi", true, true, Some(author()));
        gate.left(7, 1);
        let other = DraftAuthor { connection_id: 9, ..author() };
        gate.input(b"!", true, true, Some(other.clone()));
        let draft = gate.draft.as_ref().unwrap();
        assert_eq!((&draft.by, draft.left), (&Some(other), None));
    }

    #[test]
    fn pastes_are_bracketed_only_when_asked_and_cannot_end_early() {
        assert_eq!(paste(true, "hi"), b"\x1b[200~hi\x1b[201~");
        assert_eq!(paste(false, "hi"), b"hi");
        assert_eq!(paste(true, "a\x1b[201~b"), b"\x1b[200~ab\x1b[201~");
    }
}
