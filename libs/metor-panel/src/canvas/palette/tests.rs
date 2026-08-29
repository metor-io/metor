//! The palette's claim is that every entry inserts something that compiles
//! and is already wired, so that is what these check — by inserting each one
//! and compiling the result.

use metor_expr::{CompSchema, FrameSchema, Manifest, Resolver, Ty};

use super::*;
use crate::canvas::edit;

struct Table;

impl Resolver for Table {
    fn component(&self, path: &str) -> Option<CompSchema> {
        (path == "wheels.rpm").then_some(CompSchema { ty: Ty::F64 })
    }

    fn suffix(&self, name: &str) -> Vec<String> {
        match name {
            "rpm" => vec!["wheels.rpm".to_string()],
            _ => Vec::new(),
        }
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

/// Every source entry, inserted into an empty program, compiles and runs
/// itself — which is what `@system(rate=)` is for.
#[test]
fn every_source_entry_inserts_a_running_declaration() {
    for entry in entries(None, None) {
        let manifest = Manifest {
            compiler: metor_expr::COMPILER_VERSION,
            systems: Vec::new(),
            stages: Vec::new(),
            functions: Vec::new(),
        };
        let (source, name) = edit::insert(&manifest, "", entry.stem, &entry.template);
        let after = compile(&source);
        let system = after
            .system(&name)
            .unwrap_or_else(|| panic!("`{}` inserted no system:\n{source}", entry.label));
        assert_eq!(system.rate, Some(100.0), "`{}` is a source", entry.label);
        assert!(system.inputs.is_empty());
    }
}

/// Every transform entry, inserted against a selection, comes out already
/// reading it — there is no unwired state for the canvas to hold.
#[test]
fn every_transform_entry_inserts_something_already_wired() {
    let base = "signal = wheels.rpm * 1.0\n";
    let manifest = compile(base);
    let offered = entries(Some(&manifest), Some("signal"));
    assert!(
        offered.len() > entries(Some(&manifest), None).len(),
        "a selection is what makes transforms offerable"
    );

    for entry in offered {
        if entry.detail == "source · 100 Hz" {
            continue;
        }
        let (source, name) = edit::insert(&manifest, base, entry.stem, &entry.template);
        let after = compile(&source);
        let reads_signal = after
            .system(&name)
            .map(|s| {
                s.inputs.iter().any(|p| {
                    p.bindings[0]
                        == metor_expr::Binding::Produced {
                            system: 0,
                            field: 0,
                        }
                })
            })
            .or_else(|| {
                after.stages.iter().find(|s| s.name == name).map(|s| {
                    s.source
                        == metor_expr::Binding::Produced {
                            system: 0,
                            field: 0,
                        }
                })
            })
            .unwrap_or_else(|| {
                panic!("`{}` inserted nothing named {name}:\n{source}", entry.label)
            });
        assert!(
            reads_signal,
            "`{}` must read the selection:\n{source}",
            entry.label
        );
    }
}

/// A `def` in the module is offerable on the same terms as anything in the
/// prelude, which is what makes the palette derived rather than declared.
#[test]
fn the_module_extends_the_palette() {
    let base = "\
def half(x: f64) -> f64:
    return x * 0.5

signal = wheels.rpm * 1.0
";
    let manifest = compile(base);
    let offered = entries(Some(&manifest), Some("signal"));
    let entry = offered
        .iter()
        .find(|e| e.label == "half")
        .expect("a module function is a palette entry");

    let (source, name) = edit::insert(&manifest, base, entry.stem, &entry.template);
    let after = compile(&source);
    assert_eq!(
        after.system(&name).unwrap().inputs[0].bindings[0],
        metor_expr::Binding::Produced {
            system: 0,
            field: 0
        }
    );
}
