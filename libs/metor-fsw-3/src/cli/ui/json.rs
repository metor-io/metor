//! The NDJSON renderer for tools.
//!
//! Each event is one line, an object with `"v"` (the schema version) and
//! `"type"`.

use std::io::Write;

use serde_json::{Value, json};

use super::Reporter;
use super::event::{Event, PhaseState};

/// The schema version of each line.
pub const VERSION: u32 = 1;

/// Writes events as JSON lines to `out`.
pub struct Json<W> {
    out: W,
}

impl<W: Write> Json<W> {
    pub fn new(out: W) -> Self {
        Self { out }
    }
}

/// The JSON object for `event`, without the version.
fn object(event: &Event<'_>) -> Value {
    match event {
        Event::Phase(phase) => {
            let (state, extra) = match phase.state {
                PhaseState::Started => ("started", json!({})),
                PhaseState::Progress { done, current } => {
                    ("progress", json!({ "done": done, "current": current }))
                }
                PhaseState::Done(took) => ("done", json!({ "secs": took.as_secs_f64() })),
                PhaseState::Failed => ("failed", json!({})),
            };
            let mut value = json!({
                "type": "phase",
                "phase": phase.verb.doing().to_lowercase(),
                "subject": phase.subject,
                "state": state,
            });
            merge(&mut value, extra);
            value
        }
        Event::Running(namespaces) => json!({ "type": "running", "targets": namespaces }),
        Event::Diagnostic(diagnostic) => json!({
            "type": "diagnostic",
            "severity": "error",
            "code": diagnostic.code().map(|code| code.to_string()),
            "message": diagnostic.to_string(),
            "help": diagnostic.help().map(|help| help.to_string()),
        }),
    }
}

fn merge(into: &mut Value, from: Value) {
    if let (Value::Object(into), Value::Object(from)) = (into, from) {
        into.extend(from);
    }
}

impl<W: Write + Send> Reporter for Json<W> {
    fn event(&mut self, event: &Event<'_>) {
        let mut value = json!({ "v": VERSION });
        merge(&mut value, object(event));
        let _ = writeln!(self.out, "{value}").and_then(|()| self.out.flush());
    }
}
