//! Framed peer calls and ordered stream capability checks.
use super::*;
use std::sync::atomic::AtomicU64;
use tokio::sync::oneshot;

/// Pending reply, stream sink, and required resize/visibility/focus contracts.
type Waiting = (oneshot::Sender<Value>, Option<(Sink, bool, bool, bool)>);

/// One open line: calls go out on channel 0, replies come back by id;
/// attach streams there are relayed to viewers here.
pub struct Line {
    pub(super) frames: mpsc::UnboundedSender<Vec<u8>>,
    pub(super) pending: Mutex<HashMap<u64, Waiting>>,
    /// Stream channel there -> the viewer here.
    pub(super) streams: Mutex<HashMap<u32, Sink>>,
    pub(super) next: AtomicU64,
    /// The events subscription that keeps this machine's mirror: its call id,
    /// then its channel, and a line of events not yet whole.
    pub(super) mirror: Mutex<(u64, Option<u32>, Vec<u8>)>,
    /// What the core there said it can do (its `hello`).
    pub(super) features: Vec<String>,
}

impl Line {
    pub(super) fn new(frames: mpsc::UnboundedSender<Vec<u8>>, hello: &Value) -> Arc<Line> {
        let features = hello["features"].as_array().into_iter().flatten();
        Arc::new(Line {
            features: features.filter_map(|f| f.as_str().map(String::from)).collect(),
            frames,
            pending: Mutex::default(),
            streams: Mutex::default(),
            next: AtomicU64::new(2),
            mirror: Mutex::default(),
        })
    }

    /// A reply to one of our calls: to whoever waits for it.
    pub(super) fn take_reply(&self, mut reply: Value) {
        let Some((waiter, sink)) = reply["id"].as_u64().and_then(|id| lock(&self.pending).remove(&id)) else { return };
        if let (Some((sink, require_resize, require_visibility, require_focus)), Some(there)) =
            (sink, reply["result"]["channel"].as_u64())
        {
            if require_resize && reply["result"]["resize_notifications"] != true {
                let _ = self.frames.send(wire::encode(there as u32, wire::CLOSE, b""));
                reply = crate::rpc::response(
                    reply["id"].clone(),
                    Err(RpcError::new("unavailable", RESIZE_LOST, "attach again")),
                );
            } else if require_visibility && reply["result"]["visibility_updates"] != true {
                let _ = self.frames.send(wire::encode(there as u32, wire::CLOSE, b""));
                reply = crate::rpc::response(
                    reply["id"].clone(),
                    Err(RpcError::new("unavailable", VISIBILITY_LOST, "attach again")),
                );
            } else if require_focus
                && (!self.features.iter().any(|f| f == "viewer_focus") || reply["result"]["focus_updates"] != true)
            {
                let _ = self.frames.send(wire::encode(there as u32, wire::CLOSE, b""));
                reply = crate::rpc::response(
                    reply["id"].clone(),
                    Err(RpcError::new("unavailable", FOCUS_LOST, "attach again")),
                );
            } else {
                lock(&self.streams).insert(there as u32, sink);
            }
        }
        let _ = waiter.send(reply);
    }

    pub(super) fn message(&self, message: &Value) {
        let _ = self.frames.send(wire::encode(0, wire::DATA, message.to_string().as_bytes()));
    }

    /// A call that gets `deadline` to answer: a stuck machine never holds us.
    pub async fn call(
        &self,
        machine: &str,
        method: &str,
        params: Value,
        deadline: Duration,
    ) -> Result<Value, RpcError> {
        self.call_into(machine, method, params, None, deadline).await
    }

    /// A call whose reply may open a stream (`channel`): it goes to `sink`
    /// from its first byte.
    pub(super) async fn call_into(
        &self,
        machine: &str,
        method: &str,
        params: Value,
        sink: Option<(Sink, bool, bool, bool)>,
        deadline: Duration,
    ) -> Result<Value, RpcError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        // Too big for one frame would end the whole line: refuse just this call.
        if req.len() > wire::MAX_PAYLOAD {
            return Err(RpcError::invalid_params(format!(
                "too big to send to {machine} ({} MB at most)",
                wire::MAX_PAYLOAD >> 20
            )));
        }
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, (tx, sink));
        let _ = self.frames.send(wire::encode(0, wire::DATA, req.as_bytes()));
        let _asked = Asked { line: self, id };
        let Ok(reply) = tokio::time::timeout(deadline, rx).await else {
            return Err(RpcError::new(
                "unavailable",
                format!("{machine} didn't answer within {} s", deadline.as_secs()),
                "it may be busy or stuck; hc machine ls shows its line",
            ));
        };
        let reply = reply.map_err(|_| {
            RpcError::new(
                "unavailable",
                format!("lost the line to {machine} during the call"),
                "it may have happened there; check, then retry",
            )
        })?;
        match reply.get("error") {
            Some(e) => Err(RpcError::from_json(e)),
            None => Ok(reply["result"].clone()),
        }
    }
}

/// A call on its way: given up on (its deadline passed, or its caller went
/// away) before the reply came, the other side hears so and can stop it. A
/// call no longer pending was answered, or its line is gone.
struct Asked<'a> {
    line: &'a Line,
    id: u64,
}

impl Drop for Asked<'_> {
    fn drop(&mut self) {
        if lock(&self.line.pending).remove(&self.id).is_some() {
            self.line.message(&json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": self.id}}));
        }
    }
}
