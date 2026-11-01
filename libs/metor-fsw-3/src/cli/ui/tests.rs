use core::time::Duration;

use super::caps::{Background, Caps, Depth};
use super::event::{Event, Phase, PhaseState, Verb};
use super::human::Human;
use super::json::Json;
use super::*;

#[derive(Debug, thiserror::Error, miette::Diagnostic)]
#[error("no target `fws`")]
#[diagnostic(code(M0101), help("did you mean `fsw`?"))]
struct NoTarget;

fn script(reporter: &mut dyn Reporter) {
    let build = |state| {
        Event::Phase(Phase {
            verb: Verb::Build,
            subject: "adcs_pack",
            state,
        })
    };
    for state in [
        PhaseState::Started,
        PhaseState::Progress {
            done: 41,
            current: "nalgebra",
        },
        PhaseState::Done(Duration::from_millis(2100)),
    ] {
        reporter.event(&build(state));
    }
    reporter.event(&Event::Phase(Phase {
        verb: Verb::Evaluate,
        subject: "target.py",
        state: PhaseState::Done(Duration::from_millis(5)),
    }));
    reporter.event(&Event::Running(&["plant", "fsw"]));
    reporter.event(&Event::Diagnostic(&NoTarget));
}

fn human(caps: Caps) -> String {
    let mut out = Vec::new();
    script(&mut Human::new(&mut out, caps));
    String::from_utf8(out).expect("utf-8")
}

#[test]
fn test_plain_skips_live_lines() {
    insta::assert_snapshot!(human(Caps::plain()));
}

#[test]
fn test_truecolor() {
    let caps = Caps {
        depth: Depth::TrueColor,
        background: Background::Dark,
        ..Caps::plain()
    };
    insta::assert_snapshot!(human(caps));
}

#[test]
fn test_json_lines_carry_the_version() {
    let mut out = Vec::new();
    script(&mut Json::new(&mut out));
    let text = String::from_utf8(out).expect("utf-8");
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
        assert_eq!(value["v"], json::VERSION);
        assert!(value["type"].is_string());
    }
    insta::assert_snapshot!(text);
}

#[test]
fn test_ui_shares_one_renderer() {
    struct Count(Arc<Mutex<u32>>);
    impl Reporter for Count {
        fn event(&mut self, _event: &Event<'_>) {
            *self.0.lock().expect("not poisoned") += 1;
        }
    }
    let count = Arc::new(Mutex::new(0));
    let ui = Ui::with(Box::new(Count(count.clone())), Caps::plain());
    ui.clone().emit(Event::Running(&[]));
    ui.emit(Event::Running(&[]));
    assert_eq!(*count.lock().expect("not poisoned"), 2);
}

fn evaluate(state: PhaseState<'static>) -> Event<'static> {
    Event::Phase(Phase {
        verb: Verb::Evaluate,
        subject: "target.py",
        state,
    })
}

#[test]
fn test_output_follows_a_failed_phase() {
    let mut out = Vec::new();
    let mut human = Human::new(&mut out, Caps::plain());
    human.event(&evaluate(PhaseState::Started));
    human.event(&evaluate(PhaseState::Failed));
    human.event(&Event::Output(b"Traceback\n"));
    let text = String::from_utf8(out).expect("utf-8");
    let failed = text.find("✗ Evaluating").expect("a failed line");
    assert!(
        text[failed..].ends_with("target.py\nTraceback\n"),
        "{text:?}"
    );
}

#[test]
fn test_summary_table() {
    use super::event::{Summary, TargetSummary};
    let summary = Summary {
        took: Duration::from_millis(12_040),
        interrupted: true,
        targets: vec![
            TargetSummary {
                name: "plant".into(),
                cycles: 1440,
                max_cycle: Duration::from_micros(520),
                overruns: 0,
            },
            TargetSummary {
                name: "fsw".into(),
                cycles: 1200,
                max_cycle: Duration::from_micros(1100),
                overruns: 2,
            },
        ],
    };
    let mut out = Vec::new();
    Human::new(&mut out, Caps::plain()).event(&Event::Ended(&summary));
    insta::assert_snapshot!(String::from_utf8(out).expect("utf-8"));
}
