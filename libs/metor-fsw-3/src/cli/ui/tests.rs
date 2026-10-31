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
    script(&mut Human::new(&mut out, caps).reveal_at_once());
    String::from_utf8(out).expect("utf-8")
}

#[test]
fn test_plain_skips_live_lines() {
    insta::assert_snapshot!(human(Caps::plain()));
}

#[test]
fn test_live_truecolor() {
    let caps = Caps {
        live: true,
        depth: Depth::TrueColor,
        unicode: true,
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

fn live_caps() -> Caps {
    Caps {
        live: true,
        unicode: true,
        ..Caps::plain()
    }
}

fn evaluate(state: PhaseState<'static>) -> Event<'static> {
    Event::Phase(Phase {
        verb: Verb::Evaluate,
        subject: "target.py",
        state,
    })
}

#[test]
fn test_ticks_advance_the_spinner_in_place() {
    let mut out = Vec::new();
    let mut human = Human::new(&mut out, live_caps()).reveal_at_once();
    human.event(&evaluate(PhaseState::Started));
    human.event(&Event::Tick);
    human.event(&Event::Tick);
    human.event(&evaluate(PhaseState::Done(Duration::from_millis(1500))));
    let text = String::from_utf8(out).expect("utf-8");
    let frames: Vec<_> = ["⠋", "⠙", "⠹"].iter().map(|f| text.find(f)).collect();
    assert!(frames.iter().all(Option::is_some), "{text:?}");
    assert!(frames.windows(2).all(|w| w[0] < w[1]), "frames in order");
    assert!(
        text.ends_with("✓ Evaluated   target.py                   1.5s\n"),
        "{text:?}"
    );
    assert_eq!(text.matches('\n').count(), 1, "the live line never scrolls");
}

#[test]
fn test_a_quick_phase_never_shows_its_live_line() {
    let mut out = Vec::new();
    let mut human = Human::new(&mut out, live_caps());
    human.event(&evaluate(PhaseState::Started));
    human.event(&Event::Tick);
    human.event(&evaluate(PhaseState::Done(Duration::from_millis(3))));
    let text = String::from_utf8(out).expect("utf-8");
    assert_eq!(text, "  ✓ Evaluated   target.py\n");
}

#[test]
fn test_output_clears_the_live_line_first() {
    let mut out = Vec::new();
    let mut human = Human::new(&mut out, live_caps()).reveal_at_once();
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
