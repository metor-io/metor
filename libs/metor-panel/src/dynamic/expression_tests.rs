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
use crate::dynamic::resolver::DbResolver;

fn db_with(components: &[(&str, PrimType, &[usize])]) -> (DB, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let db = DB::create(temp.path().join("db")).unwrap();
    for (name, prim, dim) in components {
        let id = ComponentId::new(name);
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
    let id = ComponentId::new("motor.rpm");
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

/// An expression's component id comes from its content hash, so the same
/// computation lands on the same component however many views ask for it, and
/// a different one never collides.
#[test]
fn the_component_is_named_by_the_content_hash() {
    let a = expressions::component_name(crate::dynamic::NodeId(0x1234_5678_9abc_def0));
    assert_eq!(a, "expr.123456789abcdef0");
    assert_ne!(a, expressions::component_name(crate::dynamic::NodeId(1)));
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

/// The decision this phase took, verified end to end: an expression's output
/// is a *real* component, so every view that reads history — a plot's traces,
/// a monitor's sparkline — reaches it through the ordinary path.
///
/// Before this, a view-owned expression had a ring and no time series, and a
/// trace bound to one waited on `wait_for_component` forever. What follows is
/// exactly what that trace needs to exist.
#[stellarator::test]
async fn an_expression_publishes_a_real_but_hidden_component() {
    use crate::dynamic::ops::{db_source, program};
    use metor_proto::types::Timestamp;

    let (db, _temp) = db_with(&[("wheels.rpm", PrimType::F64, &[])]);
    let resolver = DbResolver::snapshot(&db);
    let compiled =
        std::sync::Arc::new(program::Compiled::expression("wheels.rpm * 2.0", &resolver).unwrap());

    let source_id = ComponentId::new("wheels.rpm");
    let name = expressions::component_name(program::field_id(
        compiled.system_hash(0, &[db_source::from_db_id(source_id)]),
        0,
    ));
    let component = ComponentId::new(&name);

    let system = program::system(
        &compiled,
        0,
        vec![program::PortSource::live(db_source::from_db(&db, source_id).unwrap())],
        program::DEFAULT_FUEL,
        None,
    )
    .unwrap();
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let _published = expressions::publish(&db, &name, field, "wheels.rpm * 2.0").unwrap();

    // The component exists, with the schema the expression computes.
    let live = db
        .with_state(|s| s.get_component(component).cloned())
        .expect("an expression registers a component");
    assert_eq!(live.schema, ComponentSchema::new(PrimType::F64, &[]));

    // ...it is labelled by the text, marked hidden, and attributed like any
    // other dynamic output...
    db.with_state(|s| {
        let meta = s.get_component_metadata(component).expect("metadata");
        assert_eq!(meta.name, "wheels.rpm * 2.0");
        assert!(meta.is_hidden(), "an expression must not appear in pickers");
        assert_eq!(meta.metadata.get("source").map(String::as_str), Some("dynamic"));
    });

    // ...and it is absent from every picker and browser, which is what makes
    // "ephemeral" mean hidden rather than unregistered.
    let listed = crate::inspector::trace_picker::list_components(&db);
    assert!(
        !listed.iter().any(|(id, _)| *id == component),
        "a hidden component must not be offered for picking"
    );

    // Finally, what a plot actually reads: history accumulating behind the id.
    let source = db.with_state(|s| s.get_component(source_id).cloned()).unwrap();
    for step in 1..=4 {
        source
            .push_buf(Timestamp(step), &f64::from(step as i32).to_le_bytes())
            .unwrap();
    }
    for _ in 0..200 {
        if live.time_series.latest().is_some() {
            break;
        }
        stellarator::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        live.time_series.latest().is_some(),
        "an expression's component must accumulate the history a plot reads"
    );
}

/// A component's id belongs to whoever created it, so it is carried from the
/// resolver that found it rather than recomputed from the path.
///
/// The db keeps a component's id and its name as independent facts — a peer
/// announces both on the wire — so a producer is free to register `imu.omega`
/// under any id it likes. Deriving one from the name is a guess, and a wrong
/// guess looks like the component is absent rather than misaddressed.
#[test]
fn a_components_id_is_carried_not_rederived() {
    let name = "imu.omega";
    // An id that has nothing to do with the name, as a producer's may not.
    let assigned = ComponentId(0x0123_4567_89ab_cdef);
    assert_ne!(assigned, ComponentId::new(name));

    let (db, _temp) = db_with_ids(&[(name, assigned, PrimType::F64, &[3])]);
    let resolver = DbResolver::snapshot(&db);

    assert!(resolver.component(name).is_some(), "it must still type");
    assert_eq!(resolver.id_of(name), Some(assigned));
    assert!(
        db.with_state(|s| s.get_component(assigned).is_some()),
        "the carried id must find the component"
    );
    assert!(
        db.with_state(|s| s.get_component(ComponentId::new(name)).is_none()),
        "and a name-derived one must not — which is what carrying avoids"
    );
}

/// Register components under ids a producer chose, rather than ids derived
/// from their names.
fn db_with_ids(
    components: &[(&str, ComponentId, PrimType, &[usize])],
) -> (DB, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let db = DB::create(temp.path().join("db")).unwrap();
    for (name, id, prim, dim) in components {
        db.with_state_mut(|state| {
            state.insert_component(*id, ComponentSchema::new(*prim, dim), &db.path)
        })
        .unwrap();
        db.with_state_mut(|state| {
            state.set_component_metadata(
                metor_proto_wkt::ComponentMetadata {
                    component_id: *id,
                    name: (*name).to_string(),
                    metadata: Default::default(),
                },
                &db.path,
            )
        })
        .unwrap();
    }
    (db, temp)
}

/// The `=` path end to end, over a name whose two hashes disagree.
///
/// This is the bug the user hit: `imu.omega` is in the half of names where
/// `persist`'s FNV-1a and `ComponentId::new` differ, so an expression naming
/// it resolved fine and then failed to find the component, reporting it as not
/// publishing. Resolution and lookup have to agree, and they only do when the
/// id is carried rather than recomputed.
#[test]
fn an_expression_finds_the_components_it_names() {
    for name in ["imu.omega", "sensor.temp", "adcs.q_b_eci", "wheels.rpm"] {
        let (db, _temp) = db_with_ids(&[(name, ComponentId::new(name), PrimType::F64, &[])]);
        let resolver = DbResolver::snapshot(&db);
        let program = metor_expr::compile_expr(&format!("{name} * 2.0"), &resolver)
            .unwrap_or_else(|d| panic!("`{name}` should compile, got:\n{d}"));

        let ports = expressions::port_components(&program.manifest, &resolver)
            .unwrap_or_else(|e| panic!("`{name}` should resolve to a component, got: {e}"));
        assert_eq!(ports, vec![ComponentId::new(name)]);
        assert!(
            db.with_state(|s| s.get_component(ports[0]).is_some()),
            "`{name}` must be found under the id the expression resolved to"
        );
    }
}

/// A name nothing publishes is still a clear diagnostic rather than a
/// misaddressed lookup.
#[test]
fn an_unknown_component_says_so() {
    let (db, _temp) = db_with_ids(&[("wheels.rpm", ComponentId::new("wheels.rpm"), PrimType::F64, &[])]);
    let resolver = DbResolver::snapshot(&db);
    // It cannot even compile, because the resolver never offered the name.
    let diags = metor_expr::compile_expr("nothing.here * 2.0", &resolver).unwrap_err();
    assert!(format!("{diags}").contains("no component"), "{diags}");
}

/// The user's report, through the exact path `resolve` takes: seed from
/// history, then follow the channel. One point and then silence is the
/// symptom; this asserts the tail.
#[stellarator::test]
async fn a_published_expression_seeds_then_tails() {
    use crate::dynamic::ops::{db_source, program};
    use metor_proto::types::Timestamp;

    let (db, _temp) = db_with_ids(&[(
        "adcs.rate",
        ComponentId::new("adcs.rate"),
        PrimType::F64,
        &[],
    )]);
    let source_id = ComponentId::new("adcs.rate");
    let source = db.with_state(|s| s.get_component(source_id).cloned()).unwrap();

    // History first, exactly as a channel that has been running.
    source.push_buf(Timestamp(100), &10.0f64.to_le_bytes()).unwrap();
    stellarator::sleep(std::time::Duration::from_millis(20)).await;

    let resolver = DbResolver::snapshot(&db);
    let compiled =
        std::sync::Arc::new(program::Compiled::expression("adcs.rate + 1.0", &resolver).unwrap());
    let name = expressions::component_name(program::field_id(
        compiled.system_hash(0, &[db_source::from_db_id(source_id)]),
        0,
    ));

    let system = program::system(
        &compiled,
        0,
        vec![program::PortSource {
            node: db_source::from_db(&db, source_id).unwrap(),
            seed: program::latest_sample(&db, source_id),
        }],
        program::DEFAULT_FUEL,
        None,
    )
    .unwrap();
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let _published =
        expressions::publish(&db, &name, field, "adcs.rate + 1.0").unwrap();

    // Then the channel keeps publishing, as the user says it does.
    for step in 1..=4 {
        source
            .push_buf(
                Timestamp(100 + step),
                &(10.0 + step as f64).to_le_bytes(),
            )
            .unwrap();
    }

    let component = db
        .with_state(|s| s.get_component(ComponentId::new(&name)).cloned())
        .expect("the expression's component");
    for _ in 0..300 {
        if component
            .time_series
            .latest()
            .is_some_and(|l| l.timestamp() == Timestamp(104))
        {
            break;
        }
        stellarator::sleep(std::time::Duration::from_millis(5)).await;
    }
    let latest = component.time_series.latest().expect("history");
    assert_eq!(
        latest.timestamp(),
        Timestamp(104),
        "the expression stopped at {:?} instead of following its input",
        latest.timestamp()
    );
    assert_eq!(f64::from_le_bytes(latest.data().try_into().unwrap()), 15.0);
}

/// The bug the user hit: a picker hands its caller a `ComponentId` and drops
/// the handle, so if nothing else owns the expression its tasks are cancelled
/// the moment the row returns.
///
/// What that looked like was an expression publishing exactly one sample —
/// its seed, written while the nodes were briefly alive — and then never
/// again, with the component sitting there and nothing writing to it. So the
/// registry owns them, and dropping every caller-side handle changes nothing.
#[stellarator::test]
async fn an_expression_keeps_running_after_its_caller_drops_it() {
    use crate::dynamic::ops::{db_source, program};
    use metor_proto::types::Timestamp;

    let (db, _temp) = db_with_ids(&[(
        "adcs.rate",
        ComponentId::new("adcs.rate"),
        PrimType::F64,
        &[],
    )]);
    let source_id = ComponentId::new("adcs.rate");
    let source = db.with_state(|s| s.get_component(source_id).cloned()).unwrap();
    source.push_buf(Timestamp(100), &10.0f64.to_le_bytes()).unwrap();
    stellarator::sleep(std::time::Duration::from_millis(20)).await;

    let resolver = DbResolver::snapshot(&db);
    let compiled =
        std::sync::Arc::new(program::Compiled::expression("adcs.rate + 1.0", &resolver).unwrap());
    let name = expressions::component_name(program::field_id(
        compiled.system_hash(0, &[db_source::from_db_id(source_id)]),
        0,
    ));

    // Build it, register it the way `resolve` does, then let every local
    // handle go — which is exactly what the picker did.
    let mut registry = expressions::Expressions::default();
    {
        let system = program::system(
            &compiled,
            0,
            vec![program::PortSource {
                node: db_source::from_db(&db, source_id).unwrap(),
                seed: program::latest_sample(&db, source_id),
            }],
            program::DEFAULT_FUEL,
            None,
        )
        .unwrap();
        let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
        let published = expressions::publish(&db, &name, field.clone(), "adcs.rate + 1.0").unwrap();
        registry.insert(expressions::Expression::new(
            published,
            ComponentId::new(&name),
            system.node,
            field,
        ));
    }
    assert!(registry.is_live(ComponentId::new(&name)));

    // The channel keeps publishing; so must the expression.
    for step in 1..=4 {
        source
            .push_buf(Timestamp(100 + step), &(10.0 + step as f64).to_le_bytes())
            .unwrap();
    }

    let component = db
        .with_state(|s| s.get_component(ComponentId::new(&name)).cloned())
        .expect("the expression's component");
    for _ in 0..300 {
        if component
            .time_series
            .latest()
            .is_some_and(|l| l.timestamp() == Timestamp(104))
        {
            break;
        }
        stellarator::sleep(std::time::Duration::from_millis(5)).await;
    }
    let latest = component.time_series.latest().expect("history");
    assert_eq!(
        latest.timestamp(),
        Timestamp(104),
        "the expression stopped at {:?} — its tasks were dropped with the handle",
        latest.timestamp()
    );
    assert_eq!(f64::from_le_bytes(latest.data().try_into().unwrap()), 15.0);
}

/// The defect behind "broadcast isn't applied": the compiler broadcasts fine,
/// but the picker committed a single trace at element zero.
///
/// `=xyz + 1.0` over a rank-1 channel is a rank-1 result, and it plots the way
/// that channel would — one trace per element. Collapsing it to element zero
/// showed one number where three were expected, which reads exactly like the
/// arithmetic never happened.
#[test]
fn an_expression_over_a_vector_plots_every_element() {
    let (db, _temp) = db_with_ids(&[("xyz", ComponentId::new("xyz"), PrimType::F64, &[3])]);

    // Stand in for the expression's own hidden component: rank-1, as
    // `xyz + 1.0` would publish.
    let out = ComponentId::new("expr.out");
    db.with_state_mut(|s| {
        s.insert_component(out, ComponentSchema::new(PrimType::F64, &[3]), &db.path)
    })
    .unwrap();

    let plotted = crate::inspector::trace_picker::expression_elements(&db, out, "=xyz + 1.0");
    assert_eq!(plotted.len(), 3, "a rank-1 expression plots each element");
    assert_eq!(
        plotted.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(plotted[0].1, "xyz + 1.0.x");
    assert_eq!(plotted[2].1, "xyz + 1.0.z");

    // A scalar expression is still exactly one trace, labelled by its text.
    let scalar = ComponentId::new("expr.scalar");
    db.with_state_mut(|s| {
        s.insert_component(scalar, ComponentSchema::new(PrimType::F64, &[]), &db.path)
    })
    .unwrap();
    let plotted = crate::inspector::trace_picker::expression_elements(&db, scalar, "=xyz[0] + 1.0");
    assert_eq!(plotted, vec![(0, "xyz[0] + 1.0".to_string())]);
}

/// A list plot draws the interior of one sample, so an expression's element
/// count is its trace length — the same rule the time-series picker follows,
/// read the way this plot reads a channel.
///
/// Taking `len` as 1 would be this plot's version of collapsing to element
/// zero: a single point where the arithmetic produced a vector.
#[stellarator::test]
async fn an_expression_fills_a_list_trace_element_by_element() {
    use crate::dynamic::ops::{db_source, program};
    use crate::views::list_plot::trace_picker::expression_len;
    use metor_proto::types::Timestamp;

    let (db, _temp) = db_with_ids(&[("xyz", ComponentId::new("xyz"), PrimType::F64, &[3])]);
    let source_id = ComponentId::new("xyz");
    let source = db.with_state(|s| s.get_component(source_id).cloned()).unwrap();

    let text = "xyz * 2.0";
    let resolver = DbResolver::snapshot(&db);
    let compiled = std::sync::Arc::new(program::Compiled::expression(text, &resolver).unwrap());
    let name = expressions::component_name(program::field_id(
        compiled.system_hash(0, &[db_source::from_db_id(source_id)]),
        0,
    ));
    let system = program::system(
        &compiled,
        0,
        vec![program::PortSource {
            node: db_source::from_db(&db, source_id).unwrap(),
            seed: program::latest_sample(&db, source_id),
        }],
        program::DEFAULT_FUEL,
        None,
    )
    .unwrap();
    let field = program::field(&compiled, 0, 0, system.node.clone()).unwrap();
    let _published = expressions::publish(&db, &name, field, text).unwrap();

    let out = ComponentId::new(&name);
    let component = db
        .with_state(|s| s.get_component(out).cloned())
        .expect("the expression's component");
    assert_eq!(
        component.schema.dim.as_slice(),
        &[3],
        "a rank-1 input times a scalar is rank-1 out"
    );

    // What the wizard binds: the hidden component, and every element of it.
    assert_eq!(
        expression_len(&db, out),
        3,
        "the trace is as long as the expression's output"
    );

    // The plot reads the whole vector out of the latest sample, so its bounds
    // span every element rather than repeating the first.
    source
        .push_buf(Timestamp(100), &sample(&[1.0, 2.0, 3.0]))
        .unwrap();
    let bounds = wait_for_bounds(&component, 3, (2.0, 6.0)).await;
    assert_eq!(bounds, (2.0, 6.0), "every element is doubled and plotted");

    // And it follows the channel: a new sample replaces the whole vector,
    // which is what a list plot is.
    source
        .push_buf(Timestamp(101), &sample(&[-4.0, 0.0, 10.0]))
        .unwrap();
    let bounds = wait_for_bounds(&component, 3, (-8.0, 20.0)).await;
    assert_eq!(bounds, (-8.0, 20.0), "the trace follows its input");

    // A scalar expression is still one point, not zero: `len` never falls to
    // nothing, because a trace with no length draws nothing at all.
    let scalar = ComponentId::new("expr.scalar");
    db.with_state_mut(|s| {
        s.insert_component(scalar, ComponentSchema::new(PrimType::F64, &[]), &db.path)
    })
    .unwrap();
    assert_eq!(expression_len(&db, scalar), 1);
}

/// Little-endian bytes for one rank-1 `f64` sample.
fn sample(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Wait for the expression's component to carry a sample whose bounds are
/// `want`, then return them.
async fn wait_for_bounds(
    component: &metor_db::Component,
    len: usize,
    want: (f64, f64),
) -> (f64, f64) {
    use crate::views::time_series::expand_latest_sample_bounds;
    let mut seen = None;
    for _ in 0..300 {
        seen = expand_latest_sample_bounds(component, len);
        if seen == Some(want) {
            break;
        }
        stellarator::sleep(std::time::Duration::from_millis(5)).await;
    }
    seen.expect("the expression published a sample")
}
