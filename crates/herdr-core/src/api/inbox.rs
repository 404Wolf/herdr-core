//! `terminal.inbox` and `terminal.ack`: a program reading its messages itself
//! (see inbox.rs).

use super::*;

/// The terminal whose inbox it is (default: the caller's own), checked to be
/// the caller when `own` (following and acknowledging are the program's alone).
fn inbox_of(core: Ctx<'_>, terminal: Option<&str>, caller: Option<&str>, own: bool) -> Result<u32, RpcError> {
    let mine = caller.filter(|c| crate::terminals::parse_id(c).is_some());
    let id = resolve(
        core,
        terminal.or(mine).ok_or_else(|| {
            RpcError::new("needs_target", "whose inbox?", "run it inside a terminal, or give --terminal")
        })?,
    )?;
    let address = crate::terminals::address(id);
    if own && mine != Some(address.as_str()) {
        return Err(RpcError::invalid_params(format!("only {address} itself follows or acknowledges its inbox")));
    }
    Ok(id)
}

pub(super) async fn terminal_inbox(core: Ctx<'_>, p: InboxParams) -> Result<InboxResult, RpcError> {
    let id = inbox_of(core, p.terminal.as_deref(), p.caller.as_deref(), p.follow)?;
    let pending = |st: &crate::runtime::State| {
        st.reg.terminals.get(&id).map(|t| crate::inbox::pending(t, p.after)).unwrap_or_default()
    };
    if !p.follow {
        return Ok(InboxResult { messages: pending(&core.lock()), channel: None });
    }
    let filter =
        crate::events::Filter { terminal: Some(crate::terminals::address(id)), inbox: true, ..Default::default() };
    let (sink, channel) = event_sink(core, &filter)?;
    // Under one lock: nothing queued between the snapshot and the subscription is lost.
    let messages = {
        let mut st = core.lock();
        let messages = pending(&st);
        core.subscribe(&mut st, filter, sink);
        messages
    };
    // Notes waiting for a parent that couldn't take them typed can go in now.
    core.changed.notify_waiters();
    if let Some(channel) = channel {
        crate::server::heartbeat(core.core.clone(), core.conn.expect("framed").clone(), channel);
    }
    Ok(InboxResult { messages, channel })
}

pub(super) async fn terminal_ack(core: Ctx<'_>, p: AckParams) -> Result<AckResult, RpcError> {
    let id = inbox_of(core, p.terminal.as_deref(), p.caller.as_deref(), true)?;
    let waiting = {
        let mut st = core.lock();
        let t = st.reg.terminals.get_mut(&id).ok_or_else(|| RpcError::new("not_found", "removed", ""))?;
        let waiting = crate::inbox::ack(t, p.id);
        core.save(&st);
        waiting
    };
    core.flush_saved().await?;
    Ok(AckResult { waiting })
}
