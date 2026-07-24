//! Optional JSONL routing log (`<data_dir>/routing.log`), gated on
//! `LENS_ROUTING_LOG=1`. A no-op when disabled, so [`emit`] is safe to call on
//! every routed decision.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use serde_json::json;

/// One routed decision, serialized as a JSON line.
pub struct RoutingEvent {
    pub session: String,
    pub tool: String,
    pub cmd: Option<String>,
    pub decision: String,
    pub reason: String,
}

/// The append handle, opened lazily on the first enabled [`emit`].
static WRITER: OnceLock<Mutex<Option<BufWriter<File>>>> = OnceLock::new();

/// Is routing logging enabled? (`LENS_ROUTING_LOG=1`)
pub fn enabled() -> bool {
    std::env::var("LENS_ROUTING_LOG").map(|v| v == "1").unwrap_or(false)
}

/// Append `e` as one JSON line to `<data_dir>/routing.log`; no-op unless enabled.
pub fn emit(data_dir: &Path, e: RoutingEvent) {
    if !enabled() {
        return;
    }
    let cell = WRITER.get_or_init(|| Mutex::new(None));
    let mut guard = match cell.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if guard.is_none() {
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(data_dir.join("routing.log"))
        {
            Ok(f) => *guard = Some(BufWriter::new(f)),
            Err(_) => return,
        }
    }
    if let Some(w) = guard.as_mut() {
        // ts + pid identify the event: rows carry no tool_input for non-Bash
        // tools, so without them two different consecutive calls to the same
        // tool serialize byte-identically and log analysis can't tell a
        // 5-file skeleton chain from one duplicated row (the v0.10 gate log
        // read as 64% "duplicates" for exactly this reason).
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let line = json!({
            "session": e.session,
            "tool": e.tool,
            "cmd": e.cmd,
            "decision": e.decision,
            "reason": e.reason,
            "ts": ts_ms,
            "pid": std::process::id(),
        });
        // Flush each line: the hook process exits without unwinding this static.
        let _ = writeln!(w, "{line}");
        let _ = w.flush();
    }
}
