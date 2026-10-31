//! Turns a child's `--format json` lines back into events on the parent's
//! `Ui`, so a multi-target run reads as one program.

use core::time::Duration;
use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use metor_proto_wkt::LogLevel;
use serde_json::Value;

use super::diagnostic::Report;
use super::ui::Ui;
use super::ui::event::{Event, LogLine, Source, TargetSummary};
use crate::coordinator::Location;

/// What a child's lines are shown through. Clones share the summary rows.
#[derive(Clone)]
pub struct Relay {
    ui: Ui,
    /// The deployment's targets, in order; a target's place picks its hue.
    namespaces: Arc<[String]>,
    rows: Arc<Mutex<Vec<TargetSummary>>>,
}

impl Relay {
    pub fn new(ui: Ui, namespaces: &[&str]) -> Self {
        Self {
            ui,
            namespaces: namespaces.iter().map(|ns| ns.to_string()).collect(),
            rows: Arc::default(),
        }
    }

    /// Shows one line of a child's stdout. A line that is not an event is
    /// shown as it is.
    pub fn line(&self, line: &str) {
        let shown = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|value| self.event(&value));
        if shown.is_none() {
            self.output(line);
        }
    }

    /// Shows one line a child wrote to stderr.
    pub fn output(&self, line: &str) {
        self.ui.emit(Event::Output(format!("{line}\n").as_bytes()));
    }

    /// Every target's summary row so far, in deployment order.
    pub fn rows(&self) -> Vec<TargetSummary> {
        let mut rows = self.rows.lock().unwrap_or_else(|e| e.into_inner()).clone();
        rows.sort_by_key(|row| self.namespaces.iter().position(|ns| *ns == row.name));
        rows
    }

    fn event(&self, value: &Value) -> Option<()> {
        match value["type"].as_str()? {
            "log" => self.log(value),
            "diagnostic" => {
                self.ui.emit(Event::Diagnostic(&report(value)?));
                Some(())
            }
            "ended" => {
                let rows = summary_rows(value)?;
                self.rows
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(rows);
                Some(())
            }
            "output" => {
                self.ui
                    .emit(Event::Output(value["text"].as_str()?.as_bytes()));
                Some(())
            }
            _ => None,
        }
    }

    fn log(&self, value: &Value) -> Option<()> {
        let target = value["target"].as_str().map(|name| {
            let hue = self
                .namespaces
                .iter()
                .position(|ns| ns == name)
                .unwrap_or(0);
            (name, hue)
        });
        let fields = fields(&value["fields"]);
        self.ui.emit(Event::Log(&LogLine {
            elapsed: value["secs"]
                .as_f64()
                .and_then(|secs| Duration::try_from_secs_f64(secs).ok()),
            level: level(value["level"].as_str()?)?,
            source: Source {
                target,
                system: value["system"].as_str()?,
            },
            message: value["message"].as_str()?,
            fields: &fields,
        }));
        Some(())
    }
}

fn level(name: &str) -> Option<LogLevel> {
    Some(match name {
        "trace" => LogLevel::Trace,
        "debug" => LogLevel::Debug,
        "info" => LogLevel::Info,
        "warn" => LogLevel::Warn,
        "error" => LogLevel::Error,
        _ => return None,
    })
}

fn fields(value: &Value) -> Vec<(Cow<'static, str>, Cow<'static, str>)> {
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    map.iter()
        .map(|(key, value)| {
            let text = value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_string);
            (key.clone().into(), text.into())
        })
        .collect()
}

fn report(value: &Value) -> Option<Report> {
    let text = |key: &str| value[key].as_str().map(str::to_string);
    let at = text("file")
        .zip(value["line"].as_u64())
        .and_then(|(file, line)| {
            Some(Location {
                file,
                line: u32::try_from(line).ok()?,
            })
        });
    Some(Report::from_parts(
        text("code")?,
        text("message")?,
        text("help"),
        at,
    ))
}

fn summary_rows(value: &Value) -> Option<Vec<TargetSummary>> {
    value["targets"]
        .as_array()?
        .iter()
        .map(|row| {
            Some(TargetSummary {
                name: row["name"].as_str()?.to_string(),
                cycles: row["cycles"].as_u64()?,
                max_cycle: Duration::try_from_secs_f64(row["max_cycle_secs"].as_f64()?).ok()?,
                overruns: row["overruns"].as_u64()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ui::Reporter;
    use crate::cli::ui::caps::Caps;

    /// Keeps a one-line description of each event.
    struct Seen(Arc<Mutex<Vec<String>>>);

    impl Reporter for Seen {
        fn event(&mut self, event: &Event<'_>) {
            let line = match event {
                Event::Log(line) => format!(
                    "log {:?} {:?}.{} {} {:?}",
                    line.level, line.source.target, line.source.system, line.message, line.fields
                ),
                Event::Diagnostic(d) => {
                    format!("diagnostic {:?} {d}", d.code().map(|c| c.to_string()))
                }
                Event::Output(bytes) => format!("output {:?}", String::from_utf8_lossy(bytes)),
                _ => "other".into(),
            };
            self.0.lock().expect("not poisoned").push(line);
        }
    }

    fn relay() -> (Relay, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let ui = Ui::with(Box::new(Seen(seen.clone())), Caps::plain());
        (Relay::new(ui, &["plant", "fsw"]), seen)
    }

    #[test]
    fn test_events_become_the_parents() {
        let (relay, seen) = relay();
        relay.line(r#"{"v":1,"type":"log","secs":1.5,"level":"warn","target":"fsw","system":"ctrl","message":"saturated","fields":{"axis":"z"}}"#);
        relay.line(r#"{"v":1,"type":"diagnostic","code":"M0501","message":"latched","help":null,"file":null,"line":null}"#);
        relay.line(r#"{"v":1,"type":"output","text":"hello\n"}"#);
        let seen = seen.lock().expect("not poisoned");
        assert_eq!(
            *seen,
            [
                r#"log Warn Some(("fsw", 1)).ctrl saturated [("axis", "z")]"#,
                r#"diagnostic Some("M0501") latched"#,
                r#"output "hello\n""#,
            ]
        );
    }

    #[test]
    fn test_anything_else_is_shown_as_it_is() {
        let (relay, seen) = relay();
        relay.line("thread 'main' panicked");
        relay.line(r#"{"v":1,"type":"log","level":"loud"}"#);
        relay.line(r#"{"v":1,"type":"mystery"}"#);
        let seen = seen.lock().expect("not poisoned");
        assert_eq!(seen.len(), 3);
        assert!(
            seen.iter().all(|line| line.starts_with("output")),
            "{seen:?}"
        );
    }

    #[test]
    fn test_summaries_gather_in_deployment_order() {
        let (relay, _) = relay();
        let ended = |name: &str, cycles| {
            format!(
                r#"{{"v":1,"type":"ended","secs":1.0,"interrupted":false,"targets":[{{"name":"{name}","cycles":{cycles},"max_cycle_secs":0.001,"overruns":0}}]}}"#
            )
        };
        relay.line(&ended("fsw", 9));
        relay.line(&ended("plant", 7));
        let names: Vec<_> = relay
            .rows()
            .into_iter()
            .map(|row| (row.name, row.cycles))
            .collect();
        assert_eq!(names, [("plant".to_string(), 7), ("fsw".to_string(), 9)]);
        relay.line(r#"{"v":1,"type":"ended","targets":[{"name":"x"}]}"#);
        assert_eq!(relay.rows().len(), 2, "a malformed row adds nothing");
    }
}
