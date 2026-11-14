//! Inboxes: messages for a terminal whose program reads them itself, out of
//! band, instead of having them typed into its input (where a person's draft
//! may sit). A program opts in by following its own inbox (`hc inbox
//! --follow`); while it does, sends and notes to it are queued here, in
//! order, until it acknowledges them. Nobody following: they are typed as
//! before.

use crate::rpc::RpcError;
use crate::runtime::{Core, State, now_ms};
use crate::terminals::Terminal;
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
    t.last_message += 1;
    let message = Message { id: t.last_message, from, text: text.to_string(), kind: kind.into(), ts };
    t.inbox.push(message.clone());
    (message, t.inbox.len())
}

/// How many inbox-only message keys a terminal remembers.
pub const KEPT_KEYS: usize = 256;
/// The longest key, in bytes.
pub const MAX_KEY: usize = 128;

/// Remembers `key` as queued on `t`, keeping the newest [`KEPT_KEYS`]. Kept
/// with the registry, so a restarted core still knows them.
pub fn remember_key(t: &mut Terminal, key: &str) {
    t.queued_keys.push(key.to_string());
    let over = t.queued_keys.len().saturating_sub(KEPT_KEYS);
    t.queued_keys.drain(..over);
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
    /// safely ask again.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_or_queue(
        &self,
        id: u32,
        from: Option<String>,
        text: &str,
        force: bool,
        only_if: Option<&str>,
        inbox_only: bool,
        key: Option<&str>,
    ) -> Result<crate::input::Sent, RpcError> {
        let queued = {
            let mut st = self.lock();
            let open = self.inbox_open(&st, id);
            let known = |t: &Terminal| key.is_some_and(|key| t.queued_keys.iter().any(|k| k == key));
            let found = st.reg.terminals.get(&id).filter(|t| known(t) || (open && t.is_live()));
            match found.map(|t| (crate::input::state_word(t), t.turn, t.reported, t.inbox.len(), known(t))) {
                Some((word, turn, reported, waiting, true)) => Some((word, turn, reported, waiting, true)),
                Some((word, turn, reported, _, false)) => {
                    crate::input::check_status(id, word.as_deref(), only_if)?;
                    if let Some(key) = key {
                        remember_key(st.reg.terminals.get_mut(&id).expect("found above"), key);
                    }
                    let waiting = self.queue_message(&mut st, id, from, text, "send");
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
            return self.send(id, text, force, only_if).await;
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
    fn a_terminal_keeps_the_newest_keys() {
        let mut r = Registry::default();
        let id = r.create(NewTerminal { argv: vec!["x".into()], ..Default::default() }, 0).unwrap();
        let t = r.terminals.get_mut(&id).unwrap();
        for n in 0..KEPT_KEYS + 10 {
            remember_key(t, &format!("k{n}"));
        }
        assert_eq!(t.queued_keys.len(), KEPT_KEYS);
        assert_eq!(t.queued_keys.first().map(String::as_str), Some("k10"));
        assert_eq!(t.queued_keys.last(), Some(&format!("k{}", KEPT_KEYS + 9)));
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
