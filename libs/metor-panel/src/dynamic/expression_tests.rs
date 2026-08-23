//! P5: the `=` tier, and the stability contract underneath it.
//!
//! The contract the plan singles out is the one an operator would never think
//! to check: a bare name resolves by unique suffix *at authoring time*, so a
//! component added tomorrow must not change what a saved binding reads. These
//! tests pin that in both directions — what the compiler records, and what a
//! saved binding does once the tree it was written against has moved on.

use metor_db::{ComponentSchema, DB};
use metor_expr::{Binding, Resolver, Ty};
use metor_proto::types::{ComponentId, PrimType};

use crate::dynamic::expressions;
use crate::dynamic::ops::persist::component_id_for_name;
use crate::dynamic::resolver::DbResolver;

fn db_with(components: &[(&str, PrimType, &[usize])]) -> (DB, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let db = DB::create(temp.path().join("db")).unwrap();
    for (name, prim, dim) in components {
        let id = ComponentId(component_id_for_name(name));
        db.with_state_mut(|state| {
            state.insert_component(id, ComponentSchema::new(*prim, dim), &db.path)
        })
        .unwrap();
        let metadata = metor_proto_wkt::ComponentMetadata {
            component_id: id,
            name: (*name).to_string(),
            metadata: Default::default(),
        };
        db.with_state_mut(|state| state.set_component_metadata(metadata, &db.path))
            .unwrap();
    }
    (db, temp)
}

/// The panel answers the compiler's three questions from the component tree,
/// and everything numeric reads as `f64` — a channel's own element type is not
/// the language's.
#[test]
fn the_resolver_offers_the_component_tree_as_the_language_sees_it() {
    let (db, _temp) = db_with(&[
        ("wheels.rpm", PrimType::F32, &[]),
        ("counter.ticks", PrimType::U16, &[]),
        ("adcs.omega_b", PrimType::F64, &[3]),
        ("adcs.safe", PrimType::Bool, &[]),
    ]);
    let resolver = DbResolver::snapshot(&db);

    assert_eq!(resolver.component("wheels.rpm").unwrap().ty, Ty::F64);
    assert_eq!(resolver.component("counter.ticks").unwrap().ty, Ty::F64);
    assert_eq!(resolver.component("adcs.safe").unwrap().ty, Ty::Bool);
    assert_eq!(
        resolver.component("adcs.omega_b").unwrap().ty,
        Ty::Tensor {
            dtype: metor_expr::Dtype::F64,
            shape: vec![3],
        }
    );
    assert!(resolver.component("nothing.here").is_none());
    assert_eq!(resolver.suffix("rpm"), vec!["wheels.rpm".to_string()]);
    assert!(resolver.suffix("nothing").is_empty());
}

/// The stability contract, forwards: what a compiled expression records is the
/// path the suffix search *found*, not the name that was typed.
#[test]
fn a_bare_name_is_recorded_as_the_path_it_resolved_to() {
    let (db, _temp) = db_with(&[("wheels.rpm", PrimType::F64, &[])]);
    let program =
        metor_expr::compile_expr("rpm * 2.0", &DbResolver::snapshot(&db)).expect("compiles");
    assert_eq!(
        program.manifest.systems[0].inputs[0].bindings[0],
        Binding::Component("wheels.rpm".into())
    );
}

/// The stability contract, backwards — the case the plan names. A second
/// `*.rpm` appearing later makes the bare name ambiguous for *new* authoring,
/// and must leave a saved layout alone.
///
/// It does, because a saved layout is read back through the recorded path.
/// Nothing re-runs the suffix search, which is the whole point of recording
/// the resolution rather than the text.
#[test]
fn a_later_ambiguity_does_not_disturb_what_was_already_resolved() {
    let (db, _temp) = db_with(&[("wheels.rpm", PrimType::F64, &[])]);
    let saved = metor_expr::compile_expr("rpm * 2.0", &DbResolver::snapshot(&db))
        .expect("compiles while the name is unique");
    let Binding::Component(recorded) = &saved.manifest.systems[0].inputs[0].bindings[0] else {
        panic!("a bare name resolves to a component");
    };
    assert_eq!(recorded, "wheels.rpm");

    // A second component ending in `.rpm` arrives.
    let id = ComponentId(component_id_for_name("motor.rpm"));
    db.with_state_mut(|state| {
        state.insert_component(id, ComponentSchema::new(PrimType::F64, &[]), &db.path)
    })
    .unwrap();
    db.with_state_mut(|state| {
        state.set_component_metadata(
            metor_proto_wkt::ComponentMetadata {
                component_id: id,
                name: "motor.rpm".to_string(),
                metadata: Default::default(),
            },
            &db.path,
        )
    })
    .unwrap();
    let crowded = DbResolver::snapshot(&db);

    // Typing it fresh is now a diagnostic that lists the candidates...
    let diags = metor_expr::compile_expr("rpm * 2.0", &crowded)
        .expect_err("the bare name is ambiguous now");
    let text = format!("{diags}");
    assert!(text.contains("ambiguous"), "{text}");
    assert!(text.contains("motor.rpm") && text.contains("wheels.rpm"), "{text}");

    // ...while the saved binding still reads exactly what it always read.
    let reloaded = metor_expr::compile_expr(recorded, &crowded)
        .expect("the recorded path is never ambiguous");
    assert_eq!(
        reloaded.manifest.systems[0].inputs[0].bindings[0],
        Binding::Component("wheels.rpm".into())
    );
}

/// A saved binding is a string, and its first character is what says whether
/// it names a component or computes one.
#[test]
fn the_sigil_is_what_separates_a_binding_from_an_expression() {
    assert!(expressions::is_expression("=adcs.omega_b * 100.0"));
    assert!(!expressions::is_expression("adcs.omega_b"));
    assert_eq!(expressions::body("=adcs.omega_b * 100.0"), "adcs.omega_b * 100.0");
    assert_eq!(expressions::body("= rpm * 2.0 "), "rpm * 2.0");
    // An unprefixed expression is still readable, so a caller may hand either.
    assert_eq!(expressions::body("rpm * 2.0"), "rpm * 2.0");
}

/// Two views typing the same expression against the same components reach the
/// same content hash, and therefore share one running system rather than each
/// starting a copy of it.
#[test]
fn the_same_expression_hashes_the_same_way() {
    let (db, _temp) = db_with(&[("wheels.rpm", PrimType::F64, &[])]);
    let resolver = DbResolver::snapshot(&db);
    let compile = |text: &str| {
        let program = metor_expr::compile_expr(text, &resolver).expect("compiles");
        crate::dynamic::ops::program::Compiled::expression(text, &resolver)
            .map(|c| c.system_hash(0, &[]))
            .map(|h| (h, program.manifest.systems[0].inputs.len()))
            .expect("builds")
    };
    assert_eq!(compile("rpm * 2.0"), compile("rpm * 2.0"));
    assert_ne!(compile("rpm * 2.0").0, compile("rpm * 3.0").0);
}
