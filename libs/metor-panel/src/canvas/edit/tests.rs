//! Every test here does the same thing: apply a gesture to source, recompile
//! the result, and assert about the *manifest* rather than about the text.
//!
//! That is deliberate. What an edit produces has to be a program that means
//! the intended thing; whether it is spelled the way a human would spell it is
//! a second question, and pinning the text would make these tests fail for
//! reasons nobody cares about. The one exception is the edit that must not
//! touch what it did not mean to, where the text is exactly the claim.

use metor_expr::{Binding, CompSchema, Decl, Dtype, FrameSchema, Manifest, Resolver, Ty};

use super::*;

struct Table;

impl Resolver for Table {
    fn component(&self, path: &str) -> Option<CompSchema> {
        let ty = match path {
            "wheels.rpm" | "wheels.torque" | "nav.attitude.rate" => Ty::F64,
            "adcs.omega_b" => Ty::Tensor {
                dtype: Dtype::F64,
                shape: vec![3],
            },
            _ => return None,
        };
        Some(CompSchema { ty })
    }

    fn suffix(&self, name: &str) -> Vec<String> {
        [
            "wheels.rpm",
            "wheels.torque",
            "nav.attitude.rate",
            "adcs.omega_b",
        ]
        .into_iter()
        .filter(|path| path.rsplit('.').next() == Some(name))
        .map(str::to_string)
        .collect()
    }

    fn frame(&self, _: &str) -> Option<FrameSchema> {
        None
    }
}

fn compile(source: &str) -> Manifest {
    metor_expr::compile_module(source, &Table)
        .unwrap_or_else(|d| panic!("expected this to compile:\n{source}\n{d}"))
        .manifest
}

/// The rule every gesture obeys: what comes out is a program, and what it
/// means is what the gesture asked for.
fn recompiled(source: &str) -> Manifest {
    compile(source)
}

fn decl_of(manifest: &Manifest, name: &str) -> Decl {
    manifest
        .declarations()
        .into_iter()
        .find(|d| match d {
            Decl::System(i) => manifest.systems[*i].name == name,
            Decl::Stage(i) => manifest.stages[*i].name == name,
        })
        .unwrap_or_else(|| panic!("no declaration `{name}`"))
}

#[test]
fn connecting_an_edge_rewrites_one_binding() {
    let source = "scaled = wheels.rpm * 2.0\n";
    let manifest = compile(source);
    let edited = connect(&manifest, source, decl_of(&manifest, "scaled"), 0, "wheels.torque")
        .expect("the port is rebindable");

    let after = recompiled(&edited);
    assert_eq!(
        after.system("scaled").unwrap().inputs[0].bindings[0],
        Binding::Component("wheels.torque".into()),
        "the port now reads what the gesture pointed it at"
    );
    // And the arithmetic is untouched — the canvas edits bindings, never
    // bodies.
    assert_eq!(edited, "scaled = wheels.torque * 2.0\n");
}

/// A port bound to a declaration is rebound the same way, which is what makes
/// an in-program edge draggable.
#[test]
fn an_edge_between_declarations_is_rebindable() {
    let source = "a = wheels.rpm * 2.0\nb = wheels.torque + 1.0\nc = a + 1.0\n";
    let manifest = compile(source);
    let edited = connect(&manifest, source, decl_of(&manifest, "c"), 0, "b").unwrap();

    let after = recompiled(&edited);
    let c = after.system("c").unwrap();
    assert_eq!(c.inputs[0].bindings[0], Binding::Produced { system: 1, field: 0 });
    assert_eq!(after.systems[1].name, "b");
}

/// A bare name in the source resolves to a full path in the manifest, and a
/// rebinding has to find the spelling the file actually uses.
#[test]
fn a_bare_name_is_rebound_where_it_is_written() {
    let source = "scaled = rpm * 2.0\n";
    let manifest = compile(source);
    assert_eq!(
        manifest.system("scaled").unwrap().inputs[0].bindings[0],
        Binding::Component("wheels.rpm".into()),
        "the manifest records what the suffix search found"
    );
    let edited = connect(&manifest, source, decl_of(&manifest, "scaled"), 0, "wheels.torque").unwrap();
    assert_eq!(edited, "scaled = wheels.torque * 2.0\n");
}

/// A rename is a migration: the declaration, and every consumer that named it.
#[test]
fn renaming_carries_its_consumers() {
    let source = "\
@system(\"wheels.rpm\")
def raw(rpm) -> f64:
    return rpm * 2.0

doubled = raw + 1.0
slow = resample_zoh(doubled, 10.0)
";
    let manifest = compile(source);
    let edited = rename(&manifest, source, decl_of(&manifest, "raw"), "scaled").unwrap();

    let after = recompiled(&edited);
    assert!(after.system("raw").is_none(), "the old name is gone");
    assert_eq!(after.system("scaled").unwrap().publishes, vec!["scaled"]);
    assert_eq!(
        after.system("doubled").unwrap().inputs[0].bindings[0],
        Binding::Produced { system: 0, field: 0 },
        "the consumer still reads it"
    );
    assert_eq!(
        after.stages[0].source,
        Binding::Produced { system: 1, field: 0 },
        "and so does the stage downstream of that"
    );
}

/// The state key follows the system name, so a rename migrates state rather
/// than resetting it — `metor_expr::state` keys on `(system, field, type)`.
#[test]
fn renaming_migrates_the_state_key() {
    let source = "\
class Lp(State):
    value: f64 = 0.0

@system(\"wheels.rpm\")
def filtered(rpm, state: Lp) -> f64:
    state.value = 0.2 * rpm + 0.8 * state.value
    return state.value
";
    let manifest = compile(source);
    let before = metor_expr::state::slots(&manifest);
    assert_eq!(before[0].key.system, "filtered");

    let edited = rename(&manifest, source, decl_of(&manifest, "filtered"), "lowpass").unwrap();
    let after = metor_expr::state::slots(&recompiled(&edited));
    assert_eq!(after[0].key.system, "lowpass");
    assert_eq!(after[0].key.field, before[0].key.field);
    assert_eq!(after[0].key.ty, before[0].key.ty);
}

/// A rename must not touch a word that merely looks like the name, which is
/// why it works from spans rather than from a search and replace.
#[test]
fn renaming_leaves_lookalikes_alone() {
    let source = "\
rate = wheels.rpm * 2.0
rate_limit = wheels.torque + 1.0
scaled = rate * 3.0
";
    let manifest = compile(source);
    let edited = rename(&manifest, source, decl_of(&manifest, "rate"), "speed").unwrap();
    assert_eq!(
        edited,
        "\
speed = wheels.rpm * 2.0
rate_limit = wheels.torque + 1.0
scaled = speed * 3.0
"
    );
}

/// A name already taken, or one that is not a name at all, is refused rather
/// than producing source that will not compile.
#[test]
fn a_rename_that_would_not_compile_is_refused() {
    let source = "a = wheels.rpm * 2.0\nb = wheels.torque * 2.0\n";
    let manifest = compile(source);
    let a = decl_of(&manifest, "a");
    assert!(rename(&manifest, source, a, "b").is_none(), "name is taken");
    assert!(rename(&manifest, source, a, "2fast").is_none());
    assert!(rename(&manifest, source, a, "").is_none());
    assert!(rename(&manifest, source, a, "a").is_none(), "already named that");
}

/// Deleting takes the declaration and its annotation, and leaves what read it
/// as an ordinary unbound-input diagnostic — the operator asked to delete one
/// thing.
#[test]
fn deleting_removes_one_declaration_and_its_position() {
    let source = "\
a = wheels.rpm * 2.0  # @node(x=10, y=20)
b = wheels.torque * 3.0
";
    let manifest = compile(source);
    let edited = delete(&manifest, source, decl_of(&manifest, "a")).unwrap();
    assert_eq!(edited, "b = wheels.torque * 3.0\n");

    // A decorated declaration loses its decorator line too.
    let source = "\
@node(x=10, y=20)
@system(\"wheels.rpm\")
def raw(rpm) -> f64:
    return rpm * 2.0

b = wheels.torque * 3.0
";
    let manifest = compile(source);
    let edited = delete(&manifest, source, decl_of(&manifest, "raw")).unwrap();
    assert!(!edited.contains("@node"), "{edited}");
    assert!(!edited.contains("def raw"), "{edited}");
    assert_eq!(recompiled(&edited).systems.len(), 1);
}

/// The palette inserts a line of Python, which is the point: there is nothing
/// the canvas can make that the text cannot.
#[test]
fn adding_from_the_palette_inserts_a_declaration() {
    let source = "scaled = wheels.rpm * 2.0\n";
    let manifest = compile(source);
    let (edited, name) = insert(&manifest, source, "scaled", "{name} = wheels.torque * 1.0");
    assert_eq!(name, "scaled2", "a taken name gets the next free one");
    let after = recompiled(&edited);
    assert_eq!(after.systems.len(), 2);
    assert_eq!(after.system("scaled2").unwrap().publishes, vec!["scaled2"]);

    // A source system inserts as a source system, rate and all.
    let (edited, name) = insert(
        &after,
        &edited,
        "signal",
        "@system(rate=10.0)\ndef {name}() -> f64:\n    return sine(1.0, 1.0)\n",
    );
    assert_eq!(name, "signal");
    assert_eq!(recompiled(&edited).system("signal").unwrap().rate, Some(10.0));
}

/// The property the whole design rests on, stated once: a gesture's output is
/// a program, and re-reading it gives back the graph the gesture described.
#[test]
fn every_gesture_round_trips_through_a_reparse() {
    let mut source = "a = wheels.rpm * 2.0\nb = a + 1.0\n".to_string();
    for step in 0..4 {
        let manifest = compile(&source);
        let decl = decl_of(&manifest, "b");
        let layout = manifest.systems[match decl {
            Decl::System(i) => i,
            Decl::Stage(_) => unreachable!("b is a binding"),
        }]
        .layout;
        source = layout.place(&source, 100.0 + step as f32 * 10.0, 50.0);

        let manifest = compile(&source);
        assert_eq!(
            manifest.system("b").unwrap().layout.position,
            Some((100.0 + step as f32 * 10.0, 50.0)),
            "step {step}: the canvas shows what the file says"
        );
        assert_eq!(manifest.systems.len(), 2, "step {step}: nothing else moved");
    }
}

/// What a gesture costs, from the pointer release to a program the runtime
/// can build. The 200 ms debounce dominates by design; this is what sits
/// behind it.
#[test]
fn gesture_latency_is_reported() {
    let source = "\
@system(\"wheels.rpm\")
def raw(rpm) -> f64:
    return rpm * 2.0

doubled = raw + 1.0
slow = resample_zoh(doubled, 10.0)
";
    let manifest = compile(source);
    let decl = decl_of(&manifest, "doubled");
    let layout = manifest.systems[1].layout;

    let bench = |label: &str, run: &dyn Fn() -> String| {
        let rounds = 200;
        let start = std::time::Instant::now();
        let mut last = String::new();
        for _ in 0..rounds {
            last = run();
        }
        let edit = start.elapsed().as_secs_f64() / rounds as f64;
        let start = std::time::Instant::now();
        for _ in 0..rounds {
            let _ = metor_expr::compile_module(&last, &Table);
        }
        let reparse = start.elapsed().as_secs_f64() / rounds as f64;
        println!(
            "{label}: edit {:.1} µs, reparse {:.1} µs, total {:.1} µs",
            edit * 1e6,
            reparse * 1e6,
            (edit + reparse) * 1e6
        );
    };

    bench("drag", &|| layout.place(source, 120.0, 40.0));
    bench("connect", &|| {
        connect(&manifest, source, decl, 0, "wheels.torque").unwrap()
    });
    bench("rename", &|| {
        rename(&manifest, source, decl_of(&manifest, "raw"), "scaled").unwrap()
    });
    bench("delete", &|| delete(&manifest, source, decl).unwrap());
    bench("add", &|| {
        insert(&manifest, source, "extra", "{name} = wheels.torque * 1.0").0
    });
}
