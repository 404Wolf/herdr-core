//! How results look to a person: rows, tables, one-line summaries.

use super::*;

/// The program's own status word, else live or how it ended, and its dot.
pub(super) fn status(t: &Value) -> (String, &'static str) {
    const HOLLOW: &str = "\x1b[2m\u{25cb}\x1b[22m";
    if t["state"] != "live" {
        let word = match (t["exit_code"].as_i64(), t["signal"].as_i64()) {
            (Some(code), _) => format!("exit {code}"),
            (_, Some(sig)) => format!("signal {sig}"),
            _ => "ended".into(),
        };
        return (word, HOLLOW);
    }
    let Some(word) = t["status"]["state"].as_str() else { return ("running".into(), HOLLOW) };
    let dot = match word {
        "working" => "\x1b[33m\u{25cf}\x1b[39m",
        "blocked" | "error" => "\x1b[31m\u{25cf}\x1b[39m",
        "done" => "\x1b[32m\u{25cf}\x1b[39m",
        _ => "\u{25cf}",
    };
    (word.into(), dot)
}

/// Who runs in a terminal: the app it reports, else its program's name.
pub(super) fn agent(t: &Value) -> String {
    let program = t["argv"][0].as_str().unwrap_or_default();
    t["status"]["app"].as_str().unwrap_or_else(|| program.rsplit('/').next().unwrap_or_default()).into()
}

/// The current folder, falling back to where the terminal started.
fn cwd(t: &Value) -> Option<&str> {
    t["foreground"]["cwd"].as_str().or_else(|| t["cwd"].as_str())
}

/// The last folder of a terminal's current cwd, `~` for home.
pub(super) fn folder(t: &Value) -> String {
    let cwd = cwd(t).unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && cwd.trim_end_matches('/') == home.trim_end_matches('/') {
        return "~".into();
    }
    cwd.trim_end_matches('/').rsplit('/').next().filter(|f| !f.is_empty()).unwrap_or(cwd).into()
}

/// The picker's orders, Tab cycles them: last status change first (a program
/// that never reported counts from its start), what needs you first, by name.
pub(super) fn orders(terminals: &[Value]) -> Vec<(&'static str, Vec<Value>)> {
    fn recent(t: &Value) -> std::cmp::Reverse<u64> {
        std::cmp::Reverse(t["status_ms"].as_u64().or(t["created_ms"].as_u64()).unwrap_or(0))
    }
    fn need(t: &Value) -> u8 {
        match status(t).0.as_str() {
            "blocked" | "error" => 0,
            "working" => 1,
            // Done stays done after you looked (hc can't tell): no more urgent than idle.
            "done" | "idle" => 2,
            _ => 3,
        }
    }
    fn label(t: &Value) -> String {
        let name = t["name"].as_str().filter(|n| !n.is_empty()).map(String::from);
        name.or_else(|| place(t)).unwrap_or_else(|| folder(t)).to_lowercase()
    }
    let by = |order: fn(&Value, &Value) -> std::cmp::Ordering| {
        let mut sorted = terminals.to_vec();
        sorted.sort_by(order);
        sorted
    };
    vec![
        ("recent", by(|a, b| recent(a).cmp(&recent(b)))),
        ("status", by(|a, b| need(a).cmp(&need(b)).then(recent(a).cmp(&recent(b))))),
        ("name", by(|a, b| label(a).cmp(&label(b)))),
    ]
}

/// The `where` label: where a viewer put it, in words ("herdr-core / review").
pub(super) fn place(t: &Value) -> Option<String> {
    t["labels"]["where"].as_str().filter(|w| !w.is_empty()).map(Into::into)
}

/// One row per terminal, children under their parent: a status dot, the
/// address, then name, agent, template and where (else folder) lined up, and
/// the status. With `header`, a dim title row goes first (for `hc ls`; the
/// picker's rows speak for themselves).
pub(super) fn items(terminals: &[Value], header: bool) -> Vec<picker::Item> {
    const TITLES: [&str; 4] = ["NAME", "AGENT", "TEMPLATE", "WHERE"];
    let cells: Vec<[String; 4]> = terminals
        .iter()
        .map(|t| {
            let place = place(t).unwrap_or_else(|| folder(t));
            [
                t["name"].as_str().unwrap_or_default().into(),
                agent(t),
                t["template"].as_str().unwrap_or_default().into(),
                place,
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..4)
        .map(|c| match cells.iter().map(|row| row[c].chars().count()).max().unwrap_or(0) {
            0 => 0,
            w if header => w.max(TITLES[c].len()),
            w => w,
        })
        .collect();
    // Template and folder are context: dimmed.
    let line = |row: &[String], dim: [bool; 4], last: &str| {
        let mut rest = String::new();
        for (c, cell) in row.iter().enumerate().filter(|(c, _)| widths[*c] > 0) {
            let pad = " ".repeat(widths[c] - cell.chars().count());
            let (on, off) = if dim[c] { ("\x1b[2m", "\x1b[22m") } else { ("", "") };
            rest.push_str(&format!("{on}{cell}{off}{pad}  "));
        }
        rest + &format!("\x1b[2m{last}\x1b[22m")
    };
    // Only built with a header: the widths fit the titles only then.
    let title = (header && !terminals.is_empty()).then(|| picker::Item {
        value: String::new(),
        mark: " ".into(),
        head: String::new(),
        rest: line(&TITLES.map(String::from), [true; 4], "STATUS"),
        short: String::new(),
        tail: "\x1b[2mSTATUS\x1b[22m".into(),
        parent: None,
    });
    let rows = terminals.iter().zip(cells).map(|(t, row)| {
        let (word, dot) = status(t);
        let place = place(t).unwrap_or_else(|| format!("{} \u{b7} {}", row[1], row[3]));
        let short =
            [row[0].as_str(), &place].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" \u{b7} ");
        picker::Item {
            value: t["address"].as_str().unwrap_or_default().into(),
            mark: dot.into(),
            head: t["address"].as_str().unwrap_or("?").into(),
            rest: line(&row, [false, false, true, false], &word),
            short,
            tail: format!("\x1b[2m{word}\x1b[22m"),
            parent: t["parent"].as_str().map(Into::into),
        }
    });
    title.into_iter().chain(rows).collect()
}

/// The facts an agent acts on; `hc get` has the full record.
pub(super) fn short(t: &Value) -> Value {
    let keep = ["address", "name", "template", "parent", "state", "turn", "exit_code", "signal", "reason", "cwd"];
    let mut out: serde_json::Map<String, Value> =
        keep.iter().filter_map(|k| t.get(*k).filter(|v| !v.is_null()).map(|v| (k.to_string(), v.clone()))).collect();
    if let Some(word) = t["status"]["state"].as_str() {
        out.insert("status".into(), word.into());
    }
    // What and where, as a person's ls shows them: agents find each other this way.
    out.insert("agent".into(), agent(t).into());
    if let Some(place) = place(t) {
        out.insert("where".into(), place.into());
    }
    Value::Object(out)
}

pub(super) fn describe_wait(r: &Value) -> String {
    let t = r["terminal"].as_str().unwrap_or("?");
    let mut lines: Vec<String> = r["children"].as_array().into_iter().flatten().map(describe_wait).collect();
    if r["reason"] == "message" || !r["from"].is_null() {
        lines.insert(0, format!("a message from {} (already in your input)", r["from"].as_str().unwrap_or("a person")));
        // A wait on one terminal, cut short: say where that one stands.
        if r["ended"] == false && lines.len() == 1 {
            let word = r["status"].as_str().unwrap_or("no status");
            lines.push(format!("{t} still running: {word}, turn {} (wait again to keep waiting)", r["turn"]));
        }
    }
    if r["reason"] == "children" && lines.is_empty() {
        return "no children are working".into();
    }
    if !lines.is_empty() {
        return lines.join("\n");
    }
    let word = r["status"].as_str().unwrap_or("no status");
    match r["reason"].as_str() {
        Some("exit") => format!("{t} exited (exit {}, turn {})", r["exit_code"], r["turn"]),
        Some("timeout") => format!("{t} still running: {word}, turn {} (wait again to keep waiting)", r["turn"]),
        Some("output") => format!("{t} printed it ({word}, turn {})", r["turn"]),
        Some("not_started") => format!("{t} hasn't started on it ({word}): hc read {t}, then hc wait {t}"),
        Some("running") => format!("{t} still running: {word}, turn {}", r["turn"]),
        _ => format!("{t} {word}, turn {}. Read: hc read {t}", r["turn"]),
    }
}

pub(super) fn summary(t: &Value) -> String {
    match t["state"].as_str() {
        Some("live") => match t["status"]["state"].as_str() {
            Some(word) => format!("live, {word} (turn {})", t["turn"]),
            None => "live".into(),
        },
        _ => {
            let reason = t["reason"].as_str().unwrap_or("finished");
            match (t["exit_code"].as_i64(), t["signal"].as_i64()) {
                (Some(code), _) => format!("finished ({reason}, exit {code})"),
                (_, Some(sig)) => format!("finished ({reason}, signal {sig})"),
                _ => format!("finished ({reason})"),
            }
        }
    }
}

pub(super) fn command(t: &Value) -> String {
    let argv: Vec<&str> = t["argv"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    argv.join(" ")
}

pub(super) fn table(r: &Value) -> String {
    let mut out = String::new();
    let cols = if std::io::stdout().is_terminal() { crate::attach::window_size().0 as usize } else { usize::MAX };
    for line in picker::table(&items(r["terminals"].as_array().map(Vec::as_slice).unwrap_or_default(), true), cols) {
        out.push_str(&line);
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("no live terminals\n");
    }
    for m in r["machines"].as_array().into_iter().flatten().filter(|m| m["state"] != "connected") {
        out.push_str(&format!("{}: {}\n", m["name"].as_str().unwrap_or("?"), line_state(m)));
    }
    match r["hidden_finished"].as_u64() {
        Some(n) if n > 0 => out.push_str(&format!("{n} finished hidden (--all)")),
        _ => {
            out.pop();
        }
    }
    out
}

/// Attached views on one local terminal and the exact current sizing decision.
pub(super) fn viewers(r: &Value) -> String {
    let grid =
        |value: &Value| format!("{}x{}", value["cols"].as_u64().unwrap_or(0), value["rows"].as_u64().unwrap_or(0));
    let leader = r["leader"].as_object().map_or_else(
        || "none".into(),
        |id| format!("c{}/ch{}", id["connection_id"].as_u64().unwrap_or(0), id["channel"].as_u64().unwrap_or(0)),
    );
    let selected = r["selected_grid"].as_object().map_or_else(|| "held".into(), |_| grid(&r["selected_grid"]));
    let mut lines = vec![format!(
        "{}  PTY {}  selected {}  leader {}",
        r["terminal"].as_str().unwrap_or("?"),
        grid(&r["current_grid"]),
        selected,
        leader
    )];
    let entries = r["viewers"].as_array().map(Vec::as_slice).unwrap_or_default();
    if entries.is_empty() {
        lines.push("no attached viewers".into());
    }
    for v in entries {
        let id = &v["identity"];
        let connection = id["connection_id"].as_u64().unwrap_or(0);
        let channel = id["channel"].as_u64().unwrap_or(0);
        let origin = match v["origin"].as_str().unwrap_or("local_client") {
            "local_desktop" => "local desktop".to_owned(),
            "relayed" => format!("relayed via {}", v["peer"].as_str().unwrap_or("?")),
            _ => "local client".to_owned(),
        };
        let pid = v["peer_pid"].as_u64().map(|pid| format!(" pid={pid}")).unwrap_or_default();
        let visibility = if v["visible"] == true { "visible" } else { "hidden" };
        let eligibility = if v["size_eligible"] == true { "eligible" } else { "excluded" };
        let capabilities = format!(
            "vis={} resize={} focus={}",
            v["visibility_updates"] == true,
            v["resize_notifications"] == true,
            v["focus_updates"] == true
        );
        let state = if v["awaiting_size"] == true {
            "awaiting-size"
        } else if v["sized"] == true {
            "sized"
        } else {
            "unsized"
        };
        let mut roles = Vec::new();
        if v["observe"] == true {
            roles.push("observe");
        }
        if v["leader"] == true {
            roles.push("leader");
        }
        if v["focused"] == true {
            roles.push("focused");
        }
        if v["leader"] == true && v["focus_updates"] == true && (v["focused"] != true || v["visible"] != true) {
            roles.push("sticky");
        }
        let group = v["desktop_group"].as_object().map_or_else(String::new, |g| {
            format!(" group={}/{}", g["line"].as_str().unwrap_or("?"), g["group"].as_str().unwrap_or("?"))
        });
        let activity = v["active_ms_ago"].as_u64().map_or_else(|| "inactive".into(), |ms| format!("active {ms}ms ago"));
        lines.push(format!(
            "c{connection}/ch{channel}  {}{}  {} {} {}  offer={}  {}  {}{}{}",
            origin,
            pid,
            visibility,
            eligibility,
            state,
            grid(&v["offered"]),
            capabilities,
            activity,
            group,
            if roles.is_empty() { String::new() } else { format!("  [{}]", roles.join(",")) }
        ));
    }
    lines.join("\n")
}

/// How a machine's line is: "connected", "unreachable since 14:02 (why)", ...
pub(super) fn line_state(m: &Value) -> String {
    let since = m["since_ms"].as_u64().map(|ms| {
        let ago = crate::runtime::now_ms().saturating_sub(ms) / 1000;
        match ago {
            0..60 => format!(" for {ago}s"),
            60..3600 => format!(" for {}m", ago / 60),
            _ => format!(" for {}h", ago / 3600),
        }
    });
    let why = m["why"].as_str().map(|w| format!(" ({w})")).unwrap_or_default();
    match m["state"].as_str() {
        Some("needs_login") => {
            let keys = if cfg!(windows) { "set up its SSH keys, then " } else { "" };
            format!(
                "needs login{}: {keys}hc machine login {}",
                since.unwrap_or_default(),
                m["name"].as_str().unwrap_or("?")
            )
        }
        Some("needs_update") => format!("needs an hc update: hc machine update {}", m["name"].as_str().unwrap_or("?")),
        Some(state) => format!("{state}{}{why}", since.unwrap_or_default()),
        None => "?".into(),
    }
}

pub(super) fn describe(t: &Value) -> String {
    let labels: Vec<String> = t["labels"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
        .collect();
    let mut description = format!(
        "{} {}\n  name: {}\n  parent: {}\n  command: {}\n  cwd: {}\n  pid: {}\n  labels: {}",
        t["address"].as_str().unwrap_or("?"),
        summary(t),
        t["name"].as_str().unwrap_or("-"),
        t["parent"].as_str().unwrap_or("-"),
        command(t),
        cwd(t).unwrap_or("-"),
        t["pid"],
        if labels.is_empty() { "-".into() } else { labels.join(" ") },
    );
    if let Some(first) = t.get("first_message").filter(|f| f["sent"] == false) {
        description.push_str(&format!("\n  first message NOT sent: {}", first["reason"].as_str().unwrap_or("?")));
    }
    description
}

/// An exec's output as the program wrote it, then its exit code as ours.
pub(super) fn exec_out(r: &Value) -> ! {
    use std::io::Write;
    let bytes = |key: &str| crate::put::decode(r[key].as_str().unwrap_or_default()).unwrap_or_default();
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let _ = out.write_all(&bytes("stdout")).and_then(|_| out.flush());
    let _ = err.write_all(&bytes("stderr"));
    if r["truncated"] == true {
        let _ = writeln!(err, "[hc] output cut: kept the first 8 MB of stdout, 1 MB of stderr");
    }
    let code = match (r["timed_out"].as_bool(), r["code"].as_i64(), r["signal"].as_i64()) {
        (Some(true), _, _) => {
            let _ = writeln!(err, "[hc] timed out after {} ms: ended", r["ms"]);
            124
        }
        (_, Some(code), _) => code as i32,
        (_, _, Some(signal)) => 128 + signal as i32,
        _ => 1,
    };
    let _ = err.flush();
    std::process::exit(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewers_output_reports_negotiated_capabilities_and_sticky_focus() {
        let output = viewers(&serde_json::json!({
            "terminal":"t1",
            "current_grid":{"cols":80,"rows":24},
            "selected_grid":{"cols":80,"rows":24},
            "leader":{"connection_id":3,"channel":1},
            "viewers":[{
                "identity":{"connection_id":3,"channel":1},
                "origin":"local_desktop",
                "peer_pid":42,
                "visible":true,
                "size_eligible":true,
                "sized":true,
                "awaiting_size":false,
                "visibility_updates":true,
                "focus_updates":true,
                "focused":false,
                "resize_notifications":true,
                "offered":{"cols":80,"rows":24},
                "active_ms_ago":1,
                "leader":true,
                "contributes_to_grid":true
            }]
        }));
        assert!(output.contains("vis=true resize=true focus=true"));
        assert!(output.contains("leader,sticky"));
        assert!(output.contains("c3/ch1  local desktop pid=42"));
    }
}
