//! Q5's prerequisite: `@node`, parsed and carried.
//!
//! Layout rides the declaration so that the file is the whole artifact and a
//! rename cannot orphan a position. The compiler's entire relationship with it
//! is round-tripping: it reads a position, it reports the region a drag
//! rewrites, and it spells the annotation back. Nothing here may change what a
//! system computes, and the last test is what says so.

use super::systems::{Table, build, imu_table, refuse};
use crate::{Form, compile_module};

/// Compile, place `name` at `(x, y)`, and hand back the new source.
fn moved(source: &str, name: &str, x: f32, y: f32) -> String {
    let manifest = compile_module(source, &imu_table()).unwrap().manifest;
    let layout = manifest
        .systems
        .iter()
        .find(|s| s.name == name)
        .map(|s| s.layout)
        .or_else(|| {
            manifest
                .stages
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.layout)
        })
        .unwrap_or_else(|| panic!("no declaration `{name}`"));
    layout.place(source, x, y)
}

fn position_of(source: &str, name: &str) -> Option<(f32, f32)> {
    let manifest = compile_module(source, &imu_table()).unwrap().manifest;
    manifest
        .systems
        .iter()
        .find(|s| s.name == name)
        .map(|s| s.layout.position)
        .or_else(|| {
            manifest
                .stages
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.layout.position)
        })
        .unwrap_or_else(|| panic!("no declaration `{name}`"))
}

#[test]
fn a_node_decorator_rides_the_declaration() {
    let source = "\
@node(x=240, y=120)
@system(\"wheels.rpm\")
def scaled(rpm) -> f64:
    return rpm * 2.0
";
    let manifest = compile_module(source, &imu_table()).unwrap().manifest;
    let system = manifest.system("scaled").unwrap();
    assert_eq!(system.layout.position, Some((240.0, 120.0)));
    assert_eq!(system.layout.form, Form::Decorator);
    assert_eq!(
        &source[system.layout.span.start as usize..system.layout.span.end as usize],
        "@node(x=240, y=120)\n",
        "the span a drag replaces is the whole annotation line"
    );

    // The order the two decorators are written in does not matter.
    let under = "\
@system(\"wheels.rpm\")
@node(x=10, y=20)
def scaled(rpm) -> f64:
    return rpm * 2.0
";
    assert_eq!(
        position_of(under, "scaled"),
        Some((10.0, 20.0)),
        "@node is presentation wherever it is stacked"
    );
}

/// Placing a declaration for the first time and moving one already placed are
/// the same call, because an unplaced one's span is where its annotation goes.
#[test]
fn placing_and_moving_are_one_edit() {
    let bare = "\
@system(\"wheels.rpm\")
def scaled(rpm) -> f64:
    return rpm * 2.0
";
    assert_eq!(position_of(bare, "scaled"), None);

    let placed = moved(bare, "scaled", 300.0, 40.0);
    assert!(placed.starts_with("@node(x=300, y=40)\n@system"), "{placed}");
    assert_eq!(position_of(&placed, "scaled"), Some((300.0, 40.0)));

    // Moving it again replaces the annotation rather than stacking another.
    let again = moved(&placed, "scaled", 12.0, 34.0);
    assert_eq!(again.matches("@node").count(), 1, "{again}");
    assert_eq!(position_of(&again, "scaled"), Some((12.0, 34.0)));

    // Sub-pixel drags round, because a source file is a diff.
    let rounded = moved(&again, "scaled", 12.4, 33.6);
    assert!(rounded.contains("@node(x=12, y=34)"), "{rounded}");
}

/// A binding is not a `def`, so Python has no decorator to hang a position on
/// — the same annotation rides as a trailing comment, and behaves the same.
#[test]
fn a_binding_carries_its_position_in_a_comment() {
    let source = "scaled = wheels.rpm * 2.0  # @node(x=80, y=160)\n";
    let manifest = compile_module(source, &imu_table()).unwrap().manifest;
    let system = manifest.system("scaled").unwrap();
    assert_eq!(system.layout.position, Some((80.0, 160.0)));
    assert_eq!(system.layout.form, Form::Comment);

    let bare = "scaled = wheels.rpm * 2.0\n";
    assert_eq!(position_of(bare, "scaled"), None);
    let placed = moved(bare, "scaled", 5.0, 6.0);
    assert_eq!(placed, "scaled = wheels.rpm * 2.0  # @node(x=5, y=6)\n");
    assert_eq!(position_of(&placed, "scaled"), Some((5.0, 6.0)));

    let again = moved(&placed, "scaled", 7.0, 8.0);
    assert_eq!(again, "scaled = wheels.rpm * 2.0  # @node(x=7, y=8)\n");
}

/// A stage is a binding too, and gets the same treatment.
#[test]
fn a_stage_is_placed_the_same_way() {
    let source = "slow = resample_zoh(wheels.rpm, 10.0)\n";
    assert_eq!(position_of(source, "slow"), None);
    let placed = moved(source, "slow", 44.0, 55.0);
    assert_eq!(
        placed,
        "slow = resample_zoh(wheels.rpm, 10.0)  # @node(x=44, y=55)\n"
    );
    assert_eq!(position_of(&placed, "slow"), Some((44.0, 55.0)));
}

/// Every declaration in a module moves independently, and moving one leaves
/// the others' text alone — which is what makes a drag a one-line diff.
#[test]
fn moving_one_card_touches_one_line() {
    let source = "\
@system(\"wheels.rpm\")
def a(rpm) -> f64:
    return rpm * 2.0

b = a + 1.0
";
    let placed = moved(source, "b", 90.0, 10.0);
    assert_eq!(
        placed,
        "\
@system(\"wheels.rpm\")
def a(rpm) -> f64:
    return rpm * 2.0

b = a + 1.0  # @node(x=90, y=10)
"
    );
    assert_eq!(position_of(&placed, "a"), None);
    assert_eq!(position_of(&placed, "b"), Some((90.0, 10.0)));
}

/// The compiler carries `@node` and looks at nothing else about it: a placed
/// system computes exactly what an unplaced one does.
#[test]
fn a_position_changes_nothing_a_system_computes() {
    let bare = "\
@system(\"wheels.rpm\")
def scaled(rpm) -> f64:
    return rpm * 2.0
";
    let placed = moved(bare, "scaled", 300.0, 40.0);
    for source in [bare, placed.as_str()] {
        let mut run = build(source, &imu_table(), "scaled");
        run.set("rpm", "rpm", &[21.0]);
        run.eval(0);
        assert_eq!(run.scalar("scaled"), 42.0);
    }

    // And a malformed one is a diagnostic rather than a silent default.
    for bad in ["@node(x=1)", "@node(x=1, y=\"far\")", "@node(x=1, y=2, z=3)"] {
        let text = refuse(
            &format!("{bad}\n@system\ndef f() -> f64:\n    return 1.0\n"),
            &Table::new(&[]),
        );
        assert!(text.contains("@node takes"), "{bad}: {text}");
    }
}
