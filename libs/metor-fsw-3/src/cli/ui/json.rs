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

/// The JSON object for `event`, without the version, or `None` for an
/// event that only shapes text.
fn object(event: &Event<'_>) -> Option<Value> {
    Some(match event {
        Event::SourceWidth(_) | Event::Tick => return None,
        Event::Ended(summary) => json!({
            "type": "ended",
            "secs": summary.took.as_secs_f64(),
            "interrupted": summary.interrupted,
            "targets": summary.targets.iter().map(|target| json!({
                "name": target.name,
                "cycles": target.cycles,
                "max_cycle_secs": target.max_cycle.as_secs_f64(),
                "overruns": target.overruns,
            })).collect::<Vec<_>>(),
        }),
        Event::Changed(path) => json!({ "type": "changed", "path": path }),
        Event::Output(bytes) => json!({
            "type": "output",
            "text": String::from_utf8_lossy(bytes),
        }),
        Event::Phase(phase) => {
            let mut value = json!({
                "type": "phase",
                "phase": phase.verb.doing().to_lowercase(),
                "subject": phase.subject,
            });
            value["state"] = match phase.state {
                PhaseState::Started => json!("started"),
                PhaseState::Progress { done, current } => {
                    (value["done"], value["current"]) = (json!(done), json!(current));
                    json!("progress")
                }
                PhaseState::Done(took) => {
                    value["secs"] = json!(took.as_secs_f64());
                    json!("done")
                }
                PhaseState::Failed => json!("failed"),
            };
            value
        }
        Event::Running(namespaces) => json!({ "type": "running", "targets": namespaces }),
        Event::Log(line) => json!({
            "type": "log",
            "secs": line.elapsed.map(|elapsed| elapsed.as_secs_f64()),
            "level": format!("{:?}", line.level).to_lowercase(),
            "target": line.source.target.map(|(name, _)| name),
            "system": line.source.system,
            "message": line.message,
            "fields": line.fields.iter().map(|(k, v)| (k.to_string(), Value::from(v.as_ref()))).collect::<serde_json::Map<_, _>>(),
        }),
        Event::Diagnostic(diagnostic) => {
            let (file, line) = location(*diagnostic).unzip();
            json!({
                "type": "diagnostic",
                "severity": "error",
                "code": diagnostic.code().map(|code| code.to_string()),
                "message": diagnostic.to_string(),
                "help": diagnostic.help().map(|help| help.to_string()),
                "file": file,
                "line": line,
            })
        }
        Event::Data(kind, value) => json!({ "type": kind, *kind: value }),
    })
}

/// The file and 1-based line of `diagnostic`'s first label.
fn location(diagnostic: &dyn miette::Diagnostic) -> Option<(String, usize)> {
    let label = diagnostic.labels()?.next()?;
    let contents = diagnostic
        .source_code()?
        .read_span(label.inner(), 0, 0)
        .ok()?;
    Some((contents.name()?.to_string(), contents.line() + 1))
}

impl<W: Write + Send> Reporter for Json<W> {
    fn event(&mut self, event: &Event<'_>) {
        let Some(mut value) = object(event) else {
            return;
        };
        value["v"] = json!(VERSION);
        let _ = writeln!(self.out, "{value}").and_then(|()| self.out.flush());
    }
}
