//! `hc inbox`: a program reading the messages for its terminal itself.

use super::*;
use crate::api::{AckParams, InboxParams};

#[derive(Subcommand)]
pub(super) enum InboxCmd {
    /// Acknowledge messages up to and including an id: they leave the inbox.
    Ack(AckParams),
}

/// One message: a JSON line when piped, else `id from: text`.
fn print(m: &Value, json: bool) {
    if json {
        println!("{m}");
    } else {
        let from = m["from"].as_str().unwrap_or(m["kind"].as_str().unwrap_or("?"));
        println!("{} {from}: {}", m["id"], m["text"].as_str().unwrap_or(""));
    }
}

pub(super) fn inbox(
    paths: &Paths,
    action: Option<InboxCmd>,
    mut p: InboxParams,
    json: bool,
) -> Result<Option<Out>, RpcError> {
    if let Some(InboxCmd::Ack(mut a)) = action {
        a.caller = Some(caller());
        let r = client::call(&client::connect_or_start(paths)?, "terminal.ack", to(&a))?;
        let human = format!("{} waiting", r["waiting"]);
        return Ok(Some(Out { json: r, human }));
    }
    p.caller = Some(caller());
    let stream = client::connect_or_start(paths)?;
    let r = client::call(&stream, "terminal.inbox", to(&p))?;
    let mut last = p.after;
    for m in r["messages"].as_array().into_iter().flatten() {
        print(m, json);
        last = last.max(m["id"].as_u64().unwrap_or(0));
    }
    if !p.follow {
        return Ok(None);
    }
    use std::io::BufRead;
    for line in std::io::BufReader::new(&stream).lines() {
        let Ok(line) = line else { break };
        let msg: Value = serde_json::from_str(&line).unwrap_or_default();
        let fresh = match msg["method"].as_str() {
            Some("event") if msg["params"]["type"] == "message" => vec![msg["params"]["data"].clone()],
            // Fell behind: what is still waiting, from the inbox itself.
            Some("missed") => {
                let again = InboxParams { after: last, follow: false, ..p };
                let r = client::call(&client::connect_or_start(paths)?, "terminal.inbox", to(&again))?;
                p = InboxParams { follow: true, ..again };
                r["messages"].as_array().cloned().unwrap_or_default()
            }
            _ => continue,
        };
        for m in &fresh {
            let id = m["id"].as_u64().unwrap_or(0);
            if id > last {
                print(m, json);
                last = id;
            }
        }
    }
    Ok(None)
}
