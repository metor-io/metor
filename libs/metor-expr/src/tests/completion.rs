//! The completion engine, probed with `$0` cursors.
//!
//! The one-rule-per-crate harness note in [`tests`](crate::tests) does not
//! apply here: completion produces no module to run. What these tests pin is
//! the *classification* — for a source with the cursor marked `$0`, which
//! candidates are offered and what range accepting one replaces. That range
//! is the contract the hosts splice text by, so it is asserted exactly.

use crate::complete::{CompletionKind, Completions, Scope, complete};
use crate::{CompSchema, FrameSchema, Resolver, Ty};

/// A component tree of two vectors and a scalar, enough for every shape of
/// name the language resolves.
struct Table;

const PATHS: &[(&str, Ty)] = &[
    (
        "adcs.omega_b",
        Ty::Tensor {
            dtype: crate::Dtype::F64,
            shape: vec![],
        },
    ),
    (
        "adcs.omega_ref",
        Ty::Tensor {
            dtype: crate::Dtype::F64,
            shape: vec![],
        },
    ),
    ("power.bus_v", Ty::F64),
    // The Imu frame's binding targets, for the @system fixtures.
    ("imu.omega", Ty::F64),
    ("imu.accel", Ty::F64),
];

impl Resolver for Table {
    fn component(&self, path: &str) -> Option<CompSchema> {
        PATHS
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, ty)| CompSchema {
                ty: ty.clone(),
                timestamp: true,
            })
    }

    fn suffix(&self, name: &str) -> Vec<String> {
        let tail = format!(".{name}");
        PATHS
            .iter()
            .map(|(p, _)| p.to_string())
            .filter(|p| p.ends_with(&tail) || p == name)
            .collect()
    }

    fn frame(&self, _name: &str) -> Option<FrameSchema> {
        None
    }

    fn paths(&self) -> Vec<String> {
        PATHS.iter().map(|(p, _)| p.to_string()).collect()
    }
}

/// Run `complete` on a source whose cursor is spelled `$0`.
fn at(source: &str, scope: Scope) -> Completions {
    let cursor = source.find("$0").expect("fixture needs a $0 cursor");
    let source = source.replace("$0", "");
    complete(&source, cursor as u32, scope, &Table, None)
}

/// As [`at`], with the module compiled first so the manifest is available —
/// the canvas's situation, where the last good compile supplies the symbols.
fn at_with_manifest(source: &str, module: &str) -> Completions {
    let manifest = crate::compile_module(module, &Table)
        .expect("fixture module must compile")
        .manifest;
    let cursor = source.find("$0").expect("fixture needs a $0 cursor");
    let source = source.replace("$0", "");
    complete(
        &source,
        cursor as u32,
        Scope::Module,
        &Table,
        Some(&manifest),
    )
}

fn labels(c: &Completions) -> Vec<&str> {
    c.items.iter().map(|i| i.label.as_str()).collect()
}

fn has(c: &Completions, label: &str) -> bool {
    c.items.iter().any(|i| i.label == label)
}

#[test]
fn bare_prefix_offers_components_and_builtins() {
    let c = at("om$0", Scope::Expression);
    assert_eq!(c.prefix, "om");
    assert_eq!((c.replace.start, c.replace.end), (0, 2));
    assert!(has(&c, "adcs.omega_b"));
    assert!(has(&c, "sqrt"));
    assert!(has(&c, "sine"), "a bare expression is a system body");
}

#[test]
fn dotted_prefix_replaces_the_whole_chain() {
    let c = at("adcs.om$0", Scope::Expression);
    assert_eq!(c.prefix, "adcs.om");
    assert_eq!((c.replace.start, c.replace.end), (0, 7));
    assert!(has(&c, "adcs.omega_b"));
}

#[test]
fn trailing_dot_keeps_the_chain() {
    let c = at("adcs.$0", Scope::Expression);
    assert_eq!(c.prefix, "adcs.");
    assert_eq!((c.replace.start, c.replace.end), (0, 5));
    assert!(has(&c, "adcs.omega_b"));
}

#[test]
fn mid_expression_prefix_is_the_trailing_token() {
    let c = at("adcs.omega_b + om$0", Scope::Expression);
    assert_eq!(c.prefix, "om");
    assert_eq!((c.replace.start, c.replace.end), (15, 17));
}

#[test]
fn open_call_still_offers() {
    let c = at("max(power.bus_v, $0", Scope::Expression);
    assert_eq!(c.prefix, "");
    assert_eq!((c.replace.start, c.replace.end), (17, 17));
    assert!(has(&c, "power.bus_v"));
}

#[test]
fn callables_bring_parens_unless_present() {
    let c = at("sq$0", Scope::Expression);
    let sqrt = c.items.iter().find(|i| i.label == "sqrt").unwrap();
    assert_eq!(sqrt.insert, "sqrt()");
    assert_eq!(sqrt.caret, Some(5));

    let c = at("sq$0(2.0)", Scope::Expression);
    let sqrt = c.items.iter().find(|i| i.label == "sqrt").unwrap();
    assert_eq!(sqrt.insert, "sqrt");
    assert_eq!(sqrt.caret, None);

    let c = at("no$0", Scope::Expression);
    let now = c.items.iter().find(|i| i.label == "now").unwrap();
    assert_eq!(now.insert, "now()");
    assert_eq!(now.caret, None, "a zero-arg call is complete as inserted");
}

#[test]
fn literal_positions_offer_nothing() {
    assert!(at("x = \"ad$0\"\n", Scope::Module).items.is_empty());
    assert!(at("x = 1.$0\n", Scope::Module).items.is_empty());
    assert!(at("x = 1$0\n", Scope::Module).items.is_empty());
    assert!(at("# ad$0\n", Scope::Module).items.is_empty());
}

#[test]
fn definition_positions_offer_nothing() {
    assert!(at("def f$0", Scope::Module).items.is_empty());
    assert!(
        at("def foo(ba$0):\n    return 1\n", Scope::Module)
            .items
            .is_empty()
    );
    assert!(
        at(
            "def foo() -> f64:\n    for i$0 in range(3):\n        pass\n    return 1.0\n",
            Scope::Module
        )
        .items
        .is_empty()
    );
}

#[test]
fn bare_dot_offers_nothing() {
    assert!(at(".$0", Scope::Expression).items.is_empty());
}

#[test]
fn module_top_level_offers_declarations_not_statement_keywords() {
    let c = at("out = om$0\n", Scope::Module);
    assert!(has(&c, "adcs.omega_b"));
    assert!(has(&c, "resample_zoh"), "resampling is a top-level binding");
    assert!(!has(&c, "sine"), "waveforms live inside systems");
    assert!(!has(&c, "return"), "{:?}", labels(&c));

    let c = at("$0", Scope::Module);
    assert!(has(&c, "def"));
    assert!(has(&c, "class"));
}

#[test]
fn helper_body_offers_params_and_keywords_not_components() {
    let module = "def scaled(x: f64) -> f64:\n    return x * 2.0\n";
    let source = "def scaled(x: f64) -> f64:\n    return $0\n";
    let c = at_with_manifest(source, module);
    assert!(has(&c, "x"), "{:?}", labels(&c));
    assert!(
        !has(&c, "adcs.omega_b"),
        "a body sees parameters, not components"
    );
    assert!(!has(&c, "sine"), "a plain def has no clock");

    let source = "def scaled(x: f64) -> f64:\n    r$0\n";
    let c = at_with_manifest(source, module);
    assert!(has(&c, "return"));
}

const SYSTEM_MODULE: &str = "\
class Imu(Frame):
    omega: f64
    accel: f64

@system
def filt(imu: Imu) -> f64:
    return imu.omega
";

#[test]
fn system_body_offers_ports_and_frame_fields() {
    let source = "\
class Imu(Frame):
    omega: f64
    accel: f64

@system
def filt(imu: Imu) -> f64:
    return im$0
";
    let c = at_with_manifest(source, SYSTEM_MODULE);
    assert!(has(&c, "imu"), "{:?}", labels(&c));
    assert!(has(&c, "sine"), "a system body has a clock");

    let source = "\
class Imu(Frame):
    omega: f64
    accel: f64

@system
def filt(imu: Imu) -> f64:
    return imu.om$0
";
    let c = at_with_manifest(source, SYSTEM_MODULE);
    assert_eq!(c.prefix, "om");
    assert_eq!(labels(&c), vec!["omega", "accel"]);
    assert!(c.items.iter().all(|i| i.kind == CompletionKind::Field));
}

#[test]
fn module_functions_and_systems_are_offered_at_top_level() {
    let module = "\
def scaled(x: f64) -> f64:
    return x * 2.0

out = scaled(power.bus_v)
";
    // The realistic shape: a new line typed at the end of the compiled
    // module, so the manifest's spans line up with the text.
    let c = at_with_manifest(&format!("{module}out2 = sc$0\n"), module);
    let scaled = c.items.iter().find(|i| i.label == "scaled").unwrap();
    assert_eq!(scaled.kind, CompletionKind::Function);
    assert_eq!(scaled.insert, "scaled()");
    assert!(has(&c, "out"), "an earlier binding is a readable channel");
}

#[test]
fn unbalanced_source_still_offers() {
    // The parser recovers; a syntax error elsewhere must not silence the
    // cursor's own position.
    let c = at("max(adcs.omega_b, om$0", Scope::Expression);
    assert_eq!(c.prefix, "om");
    assert!(has(&c, "adcs.omega_ref"));
}

#[test]
fn whitespace_cursor_offers_everything_with_empty_prefix() {
    let c = at("adcs.omega_b + $0", Scope::Expression);
    assert_eq!(c.prefix, "");
    assert!(has(&c, "power.bus_v"));
    assert!(has(&c, "True"));
}

#[test]
fn cursor_mid_identifier_replaces_from_its_start() {
    // The cursor sits inside `omega`, not at its end.
    let c = at("om$0ega", Scope::Expression);
    assert_eq!(c.prefix, "om");
    assert_eq!((c.replace.start, c.replace.end), (0, 2));
}
