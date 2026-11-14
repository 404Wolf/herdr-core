//! Inboxes: messages for a terminal whose program reads them itself, out of
//! band, instead of having them typed into its input (where a person's draft
//! may sit). A program opts in by following its own inbox (`hc inbox
//! --follow`); while it does, sends and notes to it are queued here, in
//! order, until it acknowledges them. Nobody following: they are typed as
//! before.

use crate::rpc::RpcError;
use crate::runtime::{Core, State, now_ms};
use crate::terminals::{Prompt, Terminal};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One queued message, as followers receive it.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Message {
    /// Counts up from 1 per terminal and is never reused, across restarts too.
    pub id: u64,
    /// The sending terminal (`t3`, `machine/t3`); null for a person, a script or a note.
    pub from: Option<String>,
    /// The text as it would have been typed (with its `<hc>` tag).
    pub text: String,
    /// `send` (hc send) or `note` (news about a child).
    pub kind: String,
    /// When it was queued, ms since the epoch.
    pub ts: u64,
}

/// Queues a message on `t`; returns it and how many are now waiting.
pub fn push(t: &mut Terminal, from: Option<String>, text: &str, kind: &str, ts: u64) -> (Message, usize) {
    t.prompt = Some(Prompt { by: "agent".into(), from: from.clone(), ms: ts });
    t.last_message += 1;
    let message = Message { id: t.last_message, from, text: text.to_string(), kind: kind.into(), ts };
    t.inbox.push(message.clone());
    (message, t.inbox.len())
}

/// How long a terminal remembers an inbox-only message's key, whatever
/// else is sent meanwhile: a sender may retry a send with a lost answer
/// for up to this long (advertised as `queue_keys_kept_ms` in
/// `server.status`). Kept with the registry, so across restarts too.
pub const KEYS_KEPT_MS: u64 = 60 * 60 * 1000;
/// A guard against a runaway sender: at most this many keys per terminal.
/// Far above any real use (diffd sends one per pause in a review), and
/// the oldest go first, so a key goes early only past this many sends an hour.
pub const MAX_KEYS: usize = 10_000;
/// The longest key, in bytes.
pub const MAX_KEY: usize = 128;

/// A key and when its sender first sent it (`terminal.queue`'s `key_since`).
#[derive(Clone, Copy, Debug)]
pub struct QueueKey<'a> {
    pub key: &'a str,
    pub since: u64,
}

/// Drops keys older than [`KEYS_KEPT_MS`] (and past [`MAX_KEYS`], the
/// oldest), noting from when on the terminal still has every key it
/// queued. They're kept oldest first, so only the front goes.
pub fn forget_old_keys(t: &mut Terminal, now: u64) {
    let since = now.saturating_sub(KEYS_KEPT_MS);
    while t.queued_keys.front().is_some_and(|(_, at)| *at < since) || t.queued_keys.len() > MAX_KEYS {
        if let Some((_, at)) = t.queued_keys.pop_front() {
            t.keys_complete_since = t.keys_complete_since.max(at + 1);
        }
    }
}

/// From when on `t` remembers every key it queued: a key first sent before
/// then may have been queued and forgotten, so nobody can tell.
pub fn keys_complete_since(t: &Terminal, now: u64) -> u64 {
    t.keys_complete_since.max(now.saturating_sub(KEYS_KEPT_MS))
}

/// Whether `key` was queued on `t` within [`KEYS_KEPT_MS`].
pub fn key_queued(t: &Terminal, key: &str, now: u64) -> bool {
    let since = now.saturating_sub(KEYS_KEPT_MS);
    t.queued_keys.iter().any(|(k, at)| k == key && *at >= since)
}

/// Remembers `key` as queued on `t` at `now`.
pub fn remember_key(t: &mut Terminal, key: &str, now: u64) {
    t.queued_keys.push_back((key.to_string(), now));
    forget_old_keys(t, now);
}

/// Drops every message up to and including `id`; returns how many are left.
pub fn ack(t: &mut Terminal, id: u64) -> usize {
    t.inbox.retain(|m| m.id > id);
    t.inbox.len()
}

/// The waiting messages after `after`, oldest first.
pub fn pending(t: &Terminal, after: u64) -> Vec<Message> {
    t.inbox.iter().filter(|m| m.id > after).cloned().collect()
}

impl Core {
    /// Queues a message for terminal `id` and tells its followers.
    pub fn queue_message(&self, st: &mut State, id: u32, from: Option<String>, text: &str, kind: &str) -> usize {
        let t = st.reg.terminals.get_mut(&id).expect("queued for a known terminal");
        let (message, waiting) = push(t, from, text, kind, now_ms());
        self.save(st);
        let data = serde_json::to_value(&message).expect("message serializes");
        self.emit(st, "message", id, data);
        waiting
    }

    /// `hc send`: queued when its program follows its inbox, else typed, or
    /// with `inbox_only` refused (`not_following`), never typed. A `key`
    /// (inbox-only sends) already queued on that terminal isn't queued again:
    /// the answer says `repeated`, so a sender that lost the first answer can
    /// safely ask again. A key first sent before the terminal's keys are
    /// complete (older than an hour, or past the cap) is refused with
    /// `key_forgotten`: it may have been queued, so it isn't queued now, and
    /// it isn't `not_following` either, which would say it never was.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_or_queue(
        &self,
        id: u32,
        from: Option<String>,
        text: &str,
        force: bool,
        only_if: Option<&str>,
        inbox_only: bool,
        key: Option<QueueKey<'_>>,
    ) -> Result<crate::input::Sent, RpcError> {
        let queued = {
            let mut st = self.lock();
            let open = self.inbox_open(&st, id);
            let now = now_ms();
            let known = |t: &Terminal| key.is_some_and(|k| key_queued(t, k.key, now));
            if let (Some(k), Some(t)) = (key, st.reg.terminals.get(&id))
                && !known(t)
                && k.since < keys_complete_since(t, now)
            {
                return Err(RpcError::new(
                    "key_forgotten",
                    format!("{} doesn't remember keys from when `{}` was sent", crate::terminals::address(id), k.key),
                    "nothing was queued now; it may have been before",
                ));
            }
            let found = st.reg.terminals.get(&id).filter(|t| known(t) || (open && t.is_live()));
            match found.map(|t| (crate::input::state_word(t), t.turn, t.reported, t.inbox.len(), known(t))) {
                Some((word, turn, reported, waiting, true)) => Some((word, turn, reported, waiting, true)),
                Some((word, turn, reported, _, false)) => {
                    crate::input::check_status(id, word.as_deref(), only_if)?;
                    if let Some(k) = key {
                        remember_key(st.reg.terminals.get_mut(&id).expect("found above"), k.key, now);
                    }
                    let waiting = self.queue_message(&mut st, id, from.clone(), text, "send");
                    Some((word, turn, reported, waiting, false))
                }
                None => None,
            }
        };
        let Some((status, turn, reported, waiting, repeated)) = queued else {
            if inbox_only {
                return Err(RpcError::new(
                    "not_following",
                    format!("nothing follows {}'s inbox", crate::terminals::address(id)),
                    "nothing was typed; its program doesn't read its inbox (`hc inbox --follow`)",
                ));
            }
            return self.send(id, text, force, only_if, from.as_deref()).await;
        };
        let started = if repeated { None } else { self.started_after(id, turn, reported).await };
        Ok(crate::input::Sent { status, turn, started, via: "inbox", waiting: Some(waiting), repeated })
    }

    /// Whether terminal `id`'s program follows its inbox right now.
    pub fn inbox_open(&self, st: &State, id: u32) -> bool {
        self.inbox_followed(st, &crate::terminals::address(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminals::{NewTerminal, Registry};

    #[test]
    fn a_terminal_keeps_keys_for_an_hour_however_many_follow() {
        let mut r = Registry::default();
        let id = r.create(NewTerminal { argv: vec!["x".into()], ..Default::default() }, 0).unwrap();
        let t = r.terminals.get_mut(&id).unwrap();
        let start = 1_000_000_000;
        remember_key(t, "first", start);
        // Many more keys within the hour don't push it out.
        for n in 0..1000 {
            remember_key(t, &format!("k{n}"), start + n);
        }
        let almost = start + KEYS_KEPT_MS;
        assert!(key_queued(t, "first", almost), "still known at the end of its hour");
        assert!(!key_queued(t, "first", almost + 1), "forgotten after it");
        remember_key(t, "late", almost + 1);
        assert!(!t.queued_keys.iter().any(|(k, _)| k == "first"));
        assert!(key_queued(t, "late", almost + 1));
        // A runaway sender is capped, oldest first.
        for n in 0..MAX_KEYS as u64 + 5 {
            remember_key(t, &format!("r{n}"), almost + 2);
        }
        assert_eq!(t.queued_keys.len(), MAX_KEYS);
        // Past the cap, keys sent before the newest one forgotten can't be vouched for.
        assert_eq!(keys_complete_since(t, almost + 2), almost + 3);
        // Kept with the terminal, so a restarted core still knows them.
        let saved: Terminal = serde_json::from_value(serde_json::to_value(&*t).unwrap()).unwrap();
        assert_eq!(saved.queued_keys, t.queued_keys);
    }

    #[test]
    fn messages_queue_in_order_and_leave_only_when_acknowledged() {
        let mut r = Registry::default();
        let id = r.create(NewTerminal { argv: vec!["x".into()], ..Default::default() }, 0).unwrap();
        let t = r.terminals.get_mut(&id).unwrap();
        assert_eq!(push(t, Some("t2".into()), "a", "send", 1).0.id, 1);
        assert_eq!(push(t, None, "b", "note", 2), (pending(t, 1)[0].clone(), 2));
        assert_eq!(pending(t, 0).iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(ack(t, 1), 1);
        assert_eq!(pending(t, 0)[0].text, "b");
        // Ids are never reused, even with nothing waiting (persisted with the terminal).
        ack(t, 2);
        let saved: Terminal = serde_json::from_value(serde_json::to_value(&*t).unwrap()).unwrap();
        let mut t = saved;
        assert_eq!(push(&mut t, None, "c", "send", 3).0.id, 3);
    }
}
