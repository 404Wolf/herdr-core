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

    /// `hc send`: queued when its program follows its inbox, else typed.
    pub async fn send_or_queue(
        &self,
        id: u32,
        from: Option<String>,
        text: &str,
        force: bool,
        only_if: Option<&str>,
    ) -> Result<crate::input::Sent, RpcError> {
        let queued = {
            let mut st = self.lock();
            let open = self.inbox_open(&st, id);
            let found = st.reg.terminals.get(&id).filter(|t| open && t.is_live());
            match found.map(|t| (crate::input::state_word(t), t.turn, t.reported)) {
                Some((word, turn, reported)) => {
                    crate::input::check_status(id, word.as_deref(), only_if)?;
                    let waiting = self.queue_message(&mut st, id, from, text, "send");
                    Some((word, turn, reported, waiting))
                }
                None => None,
            }
        };
        let Some((status, turn, reported, waiting)) = queued else {
            return self.send(id, text, force, only_if).await;
        };
        let started = self.started_after(id, turn, reported).await;
        Ok(crate::input::Sent { status, turn, started, via: "inbox", waiting: Some(waiting) })
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
