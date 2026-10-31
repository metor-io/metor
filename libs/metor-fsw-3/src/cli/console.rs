//! `fsw.console`, the system that brings a target's log lines to the
//! terminal.
//!
//! `metor run` appends it to each target, last in step order, with every
//! other system's `log` output as an input named after that system. Each
//! step it drains those inputs into a bounded channel. A printer thread
//! renders the lines through the [`Ui`], so the flight loop never waits on
//! the terminal.

use core::time::Duration;
use std::borrow::Cow;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use metor_proto::types::Timestamp;
use metor_proto_wkt::{LogEvent, LogLevel};

use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;

use super::ui::Ui;
use super::ui::event::{Event, LogLine, Source};
use crate::coordinator::{BuildError, InputConfig, PortRef, SystemConfig, SystemTable};
use crate::deploy::TargetConfig;
use crate::fn_system::LOG_PORT;
use crate::port::DynInputs;
use crate::record::Payload;

/// The type `fsw.console` is registered under.
pub const CONSOLE_TYPE: &str = "fsw.console";

/// The id the appended system takes, unless a system already has it.
const CONSOLE_ID: &str = "console";

/// How many lines wait for the printer before new ones are dropped.
const CAPACITY: usize = 1024;

/// What the console sends the printer.
#[derive(Debug)]
pub enum Message {
    /// A line from the `system`th input, drained in the cycle `elapsed`
    /// after the first.
    Line {
        system: usize,
        elapsed: Duration,
        event: LogEvent,
    },
    /// A `tracing` event from outside any system.
    Outside {
        level: LogLevel,
        system: Cow<'static, str>,
        message: String,
        fields: Vec<(Cow<'static, str>, Cow<'static, str>)>,
    },
    /// Lines lost to a full channel or a record that did not decode.
    Dropped(u32),
}

/// The state of `fsw.console`.
pub struct Console {
    tx: SyncSender<Message>,
    /// Lines below this level are skipped.
    min: LogLevel,
    start: Option<Timestamp>,
    dropped: u32,
}

impl Console {
    pub fn new(tx: SyncSender<Message>, min: LogLevel) -> Self {
        Self {
            tx,
            min,
            start: None,
            dropped: 0,
        }
    }

    fn send(&mut self, message: Message) {
        match self.tx.try_send(message) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.dropped = self.dropped.saturating_add(1),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn report_dropped(&mut self) {
        let dropped = std::mem::take(&mut self.dropped);
        if dropped != 0 && self.tx.try_send(Message::Dropped(dropped)).is_err() {
            self.dropped = dropped;
        }
    }
}

/// Drains every log input and sends its lines to the printer.
///
/// Drops from earlier steps are reported first, so the report gets the room
/// the printer made. Lines are timed by the cycle that drains them, since
/// threaded systems stamp theirs with wall time.
#[crate::system]
pub fn console(#[state] s: &mut Console, now: Timestamp, inputs: &mut DynInputs) {
    s.report_dropped();
    let start = *s.start.get_or_insert(now);
    let elapsed = Duration::from_micros(now.0.saturating_sub(start.0).max(0) as u64);
    for (system, (_, input)) in inputs.iter_mut().enumerate() {
        for record in input.drain() {
            let Ok(event) = record
                .and_then(|bytes| LogEvent::decode(bytes).map_err(crate::port::RecvError::Decode))
            else {
                s.dropped = s.dropped.saturating_add(1);
                continue;
            };
            if event.level < s.min {
                continue;
            }
            s.send(Message::Line {
                system,
                elapsed,
                event,
            });
        }
    }
}

/// The console's id in a target whose systems are `systems`: `console`, or
/// the first `console_N` no system has.
pub fn console_id(systems: &[String]) -> String {
    let taken = |id: &str| systems.iter().any(|system| system == id);
    core::iter::once(CONSOLE_ID.to_string())
        .chain((2..).map(|n| format!("{CONSOLE_ID}_{n}")))
        .find(|id| !taken(id))
        .unwrap_or_default()
}

/// The config of the console `id` reading `systems`.
pub fn system_config<'a>(id: &str, systems: impl IntoIterator<Item = &'a str>) -> SystemConfig {
    let inputs = systems
        .into_iter()
        .map(|system| InputConfig {
            port: system.to_string(),
            from: vec![PortRef {
                system: system.to_string(),
                port: LOG_PORT.to_string(),
            }],
        })
        .collect();
    SystemConfig {
        inputs,
        ..SystemConfig::new(id, CONSOLE_TYPE)
    }
}

/// Appends the console to `config` and registers it in `table`, sending
/// lines at `min` and above to `tx`.
///
/// Returns the ids of the systems it reads, in input order.
pub fn attach(
    config: &mut TargetConfig,
    table: &mut SystemTable,
    tx: SyncSender<Message>,
    min: LogLevel,
) -> Result<Vec<String>, BuildError> {
    let systems: Vec<String> = config
        .coordinator
        .systems
        .iter()
        .map(|system| system.id.clone())
        .collect();
    let id = console_id(&systems);
    config
        .coordinator
        .systems
        .push(system_config(&id, systems.iter().map(String::as_str)));
    table.register(CONSOLE_TYPE, console(), move || {
        Console::new(tx.clone(), min)
    })?;
    Ok(systems)
}

/// Returns the console's sender and its printer's receiver.
pub fn channel() -> (SyncSender<Message>, Receiver<Message>) {
    sync_channel(CAPACITY)
}

/// Where a target's lines say they come from.
#[derive(Clone, Debug)]
pub struct Origin {
    /// The target's namespace and hue, or `None` in a deployment of one.
    pub target: Option<(String, usize)>,
    /// The system ids, in the console's input order.
    pub systems: Vec<String>,
    /// The console's own id, the source of its drop reports.
    pub console: String,
}

/// Prints every message from `rx` through `ui` until the console is
/// dropped.
pub fn spawn_printer(rx: Receiver<Message>, origin: Origin, ui: Ui) -> JoinHandle<()> {
    std::thread::spawn(move || {
        for message in rx {
            print(&message, &origin, &ui);
        }
    })
}

fn print(message: &Message, origin: &Origin, ui: &Ui) {
    let target = origin
        .target
        .as_ref()
        .map(|(name, hue)| (name.as_str(), *hue));
    match message {
        Message::Line {
            system,
            elapsed,
            event,
        } => {
            let system = origin.systems.get(*system).map_or("?", String::as_str);
            ui.emit(Event::Log(&LogLine {
                elapsed: Some(*elapsed),
                level: event.level,
                source: Source { target, system },
                message: &event.message,
                fields: &event.fields,
            }));
        }
        Message::Outside {
            level,
            system,
            message,
            fields,
        } => ui.emit(Event::Log(&LogLine {
            elapsed: None,
            level: *level,
            source: Source { target, system },
            message,
            fields,
        })),
        Message::Dropped(count) => {
            let count = count.to_string();
            ui.emit(Event::Log(&LogLine {
                elapsed: None,
                level: LogLevel::Warn,
                source: Source {
                    target,
                    system: &origin.console,
                },
                message: "log lines dropped",
                fields: &[("dropped".into(), count.into())],
            }));
        }
    }
}

/// Where [`ConsoleLayer`] sends its lines.
#[derive(Debug, Default)]
pub struct Route {
    /// The target lines name, once the deployment is known.
    pub target: Option<(String, usize)>,
    /// The printer's channel while a run is on. Without it, lines print at
    /// once.
    pub tx: Option<SyncSender<Message>>,
}

/// A `tracing` layer that prints events from outside any system.
///
/// Events inside a system reach the terminal through its `log` port and the
/// console instead. While a run is on, lines go through the printer, so the
/// thread that logged never waits on the terminal.
pub struct ConsoleLayer {
    pub ui: Ui,
    pub route: Arc<Mutex<Route>>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ConsoleLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if crate::log::has_port() {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let message = Message::Outside {
            level: crate::log::level_of(event.metadata().level()),
            system: fields.system.unwrap_or(Cow::Borrowed("metor")),
            message: fields.message,
            fields: fields.rest,
        };
        let route = self.route.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = &route.tx {
            let _ = tx.try_send(message);
            return;
        }
        let origin = Origin {
            target: route.target.clone(),
            systems: Vec::new(),
            console: String::new(),
        };
        drop(route);
        print(&message, &origin, &self.ui);
    }
}

/// An event's `message`, its `system` if it names one, and the rest.
#[derive(Default)]
struct Fields {
    message: String,
    system: Option<Cow<'static, str>>,
    rest: Vec<(Cow<'static, str>, Cow<'static, str>)>,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        self.record_str(field, &format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "message" => self.message = value.to_string(),
            "system" => self.system = Some(value.to_string().into()),
            name => self.rest.push((name.into(), value.to_string().into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tracing_subscriber::layer::SubscriberExt;

    use super::*;
    use crate::Log;
    use crate::cli::ui::Reporter;
    use crate::cli::ui::caps::Caps;
    use crate::log::LogEventExt;

    #[crate::system]
    fn chatty(log: &mut Log) {
        log.info("hello");
        log.warn("careful");
        tracing::debug!("quiet");
        tracing::info!(k = 1, "traced");
    }

    fn target(ids: &[&str]) -> (TargetConfig, SystemTable) {
        let mut config = TargetConfig::default();
        config.coordinator.systems = ids
            .iter()
            .map(|id| SystemConfig::new(*id, "chatty"))
            .collect();
        let mut table = SystemTable::new();
        table
            .register("chatty", chatty(), || ())
            .expect("valid records");
        (config, table)
    }

    fn step_all(config: &TargetConfig, table: &SystemTable, times: &[i64]) {
        let mut coordinator = config.coordinator.clone().build(table).expect("valid");
        let subscriber = tracing_subscriber::registry().with(crate::log::LogLayer);
        tracing::subscriber::with_default(subscriber, || {
            for at in times {
                coordinator.step(Timestamp(*at));
            }
        });
    }

    fn lines(rx: &Receiver<Message>) -> Vec<(usize, Duration, String)> {
        rx.try_iter()
            .filter_map(|message| match message {
                Message::Line {
                    system,
                    elapsed,
                    event,
                } => Some((system, elapsed, event.message.into_owned())),
                Message::Outside { .. } | Message::Dropped(_) => None,
            })
            .collect()
    }

    #[test]
    fn test_lines_carry_their_system_and_elapsed_time() {
        let (mut config, mut table) = target(&["a", "b"]);
        let (tx, rx) = channel();
        let systems = attach(&mut config, &mut table, tx, LogLevel::Info).expect("attaches");
        assert_eq!(systems, ["a", "b"]);
        step_all(&config, &table, &[10, 30]);
        let seen = lines(&rx);
        let at = Duration::from_micros;
        let expected: Vec<_> = [(0, 0), (1, 0), (0, 20), (1, 20)]
            .into_iter()
            .flat_map(|(system, us)| {
                ["hello", "careful", "traced"].map(|m| (system, at(us), m.to_string()))
            })
            .collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn test_lines_below_the_level_are_skipped() {
        let (mut config, mut table) = target(&["a"]);
        let (tx, rx) = channel();
        attach(&mut config, &mut table, tx, LogLevel::Warn).expect("attaches");
        step_all(&config, &table, &[0]);
        let messages: Vec<_> = lines(&rx).into_iter().map(|(_, _, m)| m).collect();
        assert_eq!(messages, ["careful"]);
    }

    #[test]
    fn test_full_channel_reports_the_drop_once_it_has_room() {
        let (mut config, mut table) = target(&["a"]);
        let (tx, rx) = sync_channel(1);
        attach(&mut config, &mut table, tx, LogLevel::Info).expect("attaches");
        let mut coordinator = config.coordinator.build(&table).expect("valid");
        coordinator.step(Timestamp(0));
        assert_eq!(lines(&rx).len(), 1, "one of two lines fits");
        coordinator.step(Timestamp(1));
        let first = rx.try_recv().expect("the report took the room");
        assert!(matches!(first, Message::Dropped(1)), "{first:?}");
    }

    #[test]
    fn test_console_reads_no_systems_in_an_empty_target() {
        let config = system_config("console", []);
        assert!(config.inputs.is_empty());
        assert_eq!(config.ty, CONSOLE_TYPE);
    }

    /// Collects the message of every log line.
    struct Messages(Arc<Mutex<Vec<String>>>);

    impl Reporter for Messages {
        fn event(&mut self, event: &Event<'_>) {
            if let Event::Log(line) = event {
                let mut seen = self.0.lock().expect("not poisoned");
                seen.push(format!("{}:{}", line.source.system, line.message));
            }
        }
    }

    #[test]
    fn test_layer_prints_only_lines_from_outside_a_system() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let ui = Ui::with(Box::new(Messages(seen.clone())), Caps::plain());
        let layer = ConsoleLayer {
            ui,
            route: Arc::default(),
        };
        let (config, table) = target(&["a"]);
        let mut coordinator = config.coordinator.build(&table).expect("valid");
        let subscriber = tracing_subscriber::registry()
            .with(crate::log::LogLayer)
            .with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("outside");
            tracing::warn!(system = "link", "named");
            coordinator.step(Timestamp(0));
        });
        let seen = seen.lock().expect("not poisoned");
        assert_eq!(*seen, ["metor:outside", "link:named"]);
    }

    #[test]
    fn test_console_id_avoids_taken_ids() {
        let ids = |list: &[&str]| list.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        assert_eq!(console_id(&ids(&[])), "console");
        assert_eq!(console_id(&ids(&["console"])), "console_2");
        assert_eq!(console_id(&ids(&["console", "console_2"])), "console_3");
    }

    #[test]
    fn test_a_system_named_console_still_builds() {
        let (mut config, mut table) = target(&["console"]);
        let (tx, rx) = channel();
        attach(&mut config, &mut table, tx, LogLevel::Info).expect("attaches");
        assert_eq!(config.coordinator.systems[1].id, "console_2");
        step_all(&config, &table, &[0]);
        assert!(lines(&rx).iter().all(|(system, _, _)| *system == 0));
    }

    #[test]
    fn test_layer_routes_through_the_printer_during_a_run() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let ui = Ui::with(Box::new(Messages(seen.clone())), Caps::plain());
        let (tx, rx) = channel();
        let route = Arc::new(Mutex::new(Route {
            target: None,
            tx: Some(tx),
        }));
        let layer = ConsoleLayer { ui, route };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || tracing::info!("queued"));
        assert!(seen.lock().expect("not poisoned").is_empty());
        let queued = rx.try_recv().expect("one message");
        assert!(matches!(queued, Message::Outside { ref message, .. } if message == "queued"));
    }

    #[test]
    fn test_printer_names_the_target_and_reports_drops() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let ui = Ui::with(Box::new(Messages(seen.clone())), Caps::plain());
        let origin = Origin {
            target: Some(("fsw".into(), 1)),
            systems: vec!["nav".into()],
            console: "console".into(),
        };
        let (tx, rx) = channel();
        let line = LogEvent::line(Timestamp(0), LogLevel::Info, "fix", Vec::new());
        tx.send(Message::Line {
            system: 0,
            elapsed: Duration::ZERO,
            event: line,
        })
        .expect("open");
        tx.send(Message::Dropped(3)).expect("open");
        drop(tx);
        spawn_printer(rx, origin, ui)
            .join()
            .expect("the printer ends");
        assert_eq!(
            *seen.lock().expect("not poisoned"),
            ["nav:fix", "console:log lines dropped"]
        );
    }
}
