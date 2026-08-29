//! Expression binding and identity tests.

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

/// A saved resolved path is unaffected by later suffix ambiguity.
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
    assert!(
        text.contains("motor.rpm") && text.contains("wheels.rpm"),
        "{text}"
    );

    // ...while the saved binding still reads exactly what it always read.
    let reloaded =
        metor_expr::compile_expr(recorded, &crowded).expect("the recorded path is never ambiguous");
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
    assert_eq!(
        expressions::body("=adcs.omega_b * 100.0"),
        "adcs.omega_b * 100.0"
    );
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
        vec![program::PortSource::live(
            db_source::from_db(&db, source_id).unwrap(),
        )],
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
        assert_eq!(
            meta.metadata.get("source").map(String::as_str),
            Some("dynamic")
        );
    });

    // ...and it is absent from every picker and browser, which is what makes
    // "ephemeral" mean hidden rather than unregistered.
    let listed = crate::inspector::trace_picker::list_components(&db);
    assert!(
        !listed.iter().any(|(id, _)| *id == component),
        "a hidden component must not be offered for picking"
    );

    // Finally, what a plot actually reads: history accumulating behind the id.
    let source = db
        .with_state(|s| s.get_component(source_id).cloned())
        .unwrap();
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
fn db_with_ids(components: &[(&str, ComponentId, PrimType, &[usize])]) -> (DB, tempfile::TempDir) {
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

/// Expression ports retain the component id supplied by the resolver.
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
    let (db, _temp) = db_with_ids(&[(
        "wheels.rpm",
        ComponentId::new("wheels.rpm"),
        PrimType::F64,
        &[],
    )]);
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
    let source = db
        .with_state(|s| s.get_component(source_id).cloned())
        .unwrap();

    // History first, exactly as a channel that has been running.
    source
        .push_buf(Timestamp(100), &10.0f64.to_le_bytes())
        .unwrap();
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
    let _published = expressions::publish(&db, &name, field, "adcs.rate + 1.0").unwrap();

    // Then the channel keeps publishing, as the user says it does.
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
        "the expression stopped at {:?} instead of following its input",
        latest.timestamp()
    );
    assert_eq!(f64::from_le_bytes(latest.data().try_into().unwrap()), 15.0);
}

#[stellarator::test]
async fn the_registry_does_not_keep_an_unused_expression_alive() {
    use crate::dynamic::ops::{db_source, program};
    use metor_proto::types::Timestamp;

    let (db, _temp) = db_with_ids(&[(
        "adcs.rate",
        ComponentId::new("adcs.rate"),
        PrimType::F64,
        &[],
    )]);
    let source_id = ComponentId::new("adcs.rate");
    let source = db
        .with_state(|s| s.get_component(source_id).cloned())
        .unwrap();
    source
        .push_buf(Timestamp(100), &10.0f64.to_le_bytes())
        .unwrap();
    stellarator::sleep(std::time::Duration::from_millis(20)).await;

    let resolver = DbResolver::snapshot(&db);
    let compiled =
        std::sync::Arc::new(program::Compiled::expression("adcs.rate + 1.0", &resolver).unwrap());
    let name = expressions::component_name(program::field_id(
        compiled.system_hash(0, &[db_source::from_db_id(source_id)]),
        0,
    ));

    let mut registry = expressions::Expressions::default();
    let expression = {
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
        expressions::Expression::new(published, ComponentId::new(&name), system.node, field)
    };
    registry.insert(expression.clone());
    assert!(registry.is_live(ComponentId::new(&name)));
    drop(expression);
    assert!(!registry.is_live(ComponentId::new(&name)));
}

/// A vector expression creates one trace per element.
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
    let source = db
        .with_state(|s| s.get_component(source_id).cloned())
        .unwrap();

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

/// The dead-panel repro, turned green.
///
/// `ExpressionRow` hands a picker `(published_component_id, typed_text)`, and
/// what a component-bound view serializes is text. So rehydration has to
/// *resolve* that text — `ComponentId::new("=xyz * 2.0")` hashes a string
/// nobody registered, and the view binds to a component that does not exist
/// and never will. The panel looks configured and is dead.
///
/// Every one of these paths went through `ComponentId::new` on the raw text
/// before; the assertion that catches a regression is the last one in each
/// group — a view that did not resolve starts no expression, so the registry
/// stays empty.
#[gpui::test]
fn a_view_bound_to_an_expression_rehydrates_onto_what_it_publishes(cx: &mut gpui::TestAppContext) {
    use crate::dynamic::ops::{db_source, program};
    use crate::dynamic::worker::DynamicWorker;
    use crate::views::{AttitudeConfig, ComponentTextConfig, TrafficLightConfig};
    use gpui::AppContext;
    use std::sync::Arc;

    let (db, _temp) = db_with_ids(&[("xyz", ComponentId::new("xyz"), PrimType::F64, &[3])]);
    let db = Arc::new(db);
    let saved = "=xyz * 2.0";

    // What the expression publishes into, derived the way the compiler does
    // rather than from the code under test.
    let source_id = ComponentId::new("xyz");
    let resolver = DbResolver::snapshot(&db);
    let compiled = Arc::new(program::Compiled::expression("xyz * 2.0", &resolver).unwrap());
    let published = ComponentId::new(&expressions::component_name(program::field_id(
        compiled.system_hash(0, &[db_source::from_db_id(source_id)]),
        0,
    )));

    // The binding must use the expression's published component.
    assert_ne!(
        ComponentId::new(saved),
        published,
        "hashing the text is what made the panel dead"
    );

    cx.update(|cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        expressions::Expressions::init(cx);
        DynamicWorker::init(cx);

        // `bind` is the one rule, and it lands on the published component.
        let bound = expressions::bind(saved, &db, cx).expect("the expression compiles");
        assert_eq!(bound.id, published);
        assert!(bound.expression.is_some(), "an expression was started");

        // And the round trip closes: what a view saves is text `bind` will
        // recognise again, not the hash-named component.
        let text = expressions::binding_text(&db, published).expect("an expression component");
        assert_eq!(text, saved);
        assert_eq!(
            expressions::bind(&text, &db, cx).unwrap().id,
            published,
            "saving and reloading is a fixed point"
        );
    });

    // Each pane-side view, rehydrated from a config holding the text. A view
    // that resolves starts the expression; one that hashed the text would
    // leave the registry empty.
    let mut views = Vec::new();
    for (label, build) in [
        (
            "component text",
            Box::new({
                let db = db.clone();
                move |cx: &mut gpui::App| {
                    cx.new(|cx| {
                        crate::tiles::panels::TextPanel::from_config(
                            ComponentTextConfig {
                                component: saved.to_string(),
                            },
                            db.clone(),
                            cx,
                        )
                    })
                    .into_any()
                }
            }) as Box<dyn Fn(&mut gpui::App) -> gpui::AnyEntity>,
        ),
        (
            "traffic light",
            Box::new({
                let db = db.clone();
                move |cx: &mut gpui::App| {
                    cx.new(|cx| {
                        crate::tiles::panels::TrafficLightPanel::from_config(
                            TrafficLightConfig {
                                component: saved.to_string(),
                                color: None,
                            },
                            db.clone(),
                            cx,
                        )
                    })
                    .into_any()
                }
            }),
        ),
        (
            "attitude",
            Box::new({
                let db = db.clone();
                move |cx: &mut gpui::App| {
                    cx.new(|cx| {
                        crate::views::AttitudeIndicator::from_config(
                            &AttitudeConfig {
                                component: saved.to_string(),
                                ..Default::default()
                            },
                            db.clone(),
                            cx,
                        )
                    })
                    .into_any()
                }
            }),
        ),
    ] {
        let view = cx.update(|cx| {
            // A fresh registry per view, so each one is shown to start the
            // expression itself rather than inheriting the last one's.
            expressions::Expressions::init(cx);
            let view = build(cx);
            assert!(
                cx.global::<expressions::Expressions>().is_live(published),
                "{label} did not resolve its binding"
            );
            view
        });
        views.push(view);
    }

    // And the component it bound to is the one receiving data.
    let source = db
        .with_state(|s| s.get_component(source_id).cloned())
        .unwrap();
    source
        .push_buf(
            metor_proto::types::Timestamp(1),
            &[1.0f64, 2.0, 3.0]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<u8>>(),
        )
        .unwrap();

    let component = db
        .with_state(|s| s.get_component(published).cloned())
        .expect("the expression's component exists");
    for _ in 0..400 {
        if component.time_series.latest().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let latest = component
        .time_series
        .latest()
        .expect("the bound component receives data");
    let values: Vec<f64> = latest
        .data()
        .chunks_exact(8)
        .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(values, vec![2.0, 4.0, 6.0]);
}

/// Rebinding an already-placed view to an expression, and saving it.
///
/// The rebind picker sets a `ComponentId` field directly, so the live half
/// works the moment the row hands one over. What did not work was the save:
/// an instrument serializes its binding as a *name*, and an expression's
/// component is named by a content hash and labelled with the text — so
/// storing the label produced a config that rehydrated onto nothing, the same
/// dead panel one layer along.
#[gpui::test]
fn rebinding_to_an_expression_survives_the_save(cx: &mut gpui::TestAppContext) {
    use crate::dynamic::ops::{db_source, program};
    use crate::dynamic::worker::DynamicWorker;
    use crate::views::MeterConfig;
    use gpui::AppContext;
    use std::sync::Arc;

    let (db, _temp) = db_with_ids(&[("rpm", ComponentId::new("rpm"), PrimType::F64, &[])]);
    let db = Arc::new(db);
    let typed = "=rpm * 2.0";

    let source_id = ComponentId::new("rpm");
    let resolver = DbResolver::snapshot(&db);
    let compiled = Arc::new(program::Compiled::expression("rpm * 2.0", &resolver).unwrap());
    let published = ComponentId::new(&expressions::component_name(program::field_id(
        compiled.system_hash(0, &[db_source::from_db_id(source_id)]),
        0,
    )));

    let saved = cx.update(|cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        expressions::Expressions::init(cx);
        DynamicWorker::init(cx);

        // Placed against an ordinary component, as it would be.
        let meter = cx.new(|cx| {
            crate::views::Meter::from_config(
                &MeterConfig {
                    component: "rpm".to_string(),
                    ..Default::default()
                },
                db.clone(),
                cx,
            )
        });

        // Rebound to an expression, which is what the picker's row does: it
        // hands over the component the expression publishes into.
        let bound = expressions::bind(typed, &db, cx).expect("the expression compiles");
        assert_eq!(bound.id, published);
        meter.update(cx, |meter, _cx| meter.component_id = bound.id);

        // Saved. The text is what has to come back, not the label.
        meter.read(cx).to_config().component
    });

    assert_eq!(
        saved, typed,
        "a rebound expression must serialize as text `bind` will recognise"
    );

    // And reloading that config lands on the same component.
    cx.update(|cx| {
        expressions::Expressions::init(cx);
        let meter = cx.new(|cx| {
            crate::views::Meter::from_config(
                &MeterConfig {
                    component: saved.clone(),
                    ..Default::default()
                },
                db.clone(),
                cx,
            )
        });
        assert_eq!(
            meter.read(cx).component_id,
            published,
            "the reloaded meter reads what the expression publishes"
        );
        assert!(
            cx.global::<expressions::Expressions>().is_live(published),
            "and reloading started it again"
        );
    });
}

/// An expression on one axis of an XY plot, saved and reloaded.
///
/// The axes are independent, so this binds Y to an expression and leaves X on
/// an ordinary component — which is also what makes the config interesting:
/// only one of the two carries text, and the other must be untouched.
#[gpui::test]
fn an_xy_axis_binds_an_expression_independently(cx: &mut gpui::TestAppContext) {
    use crate::dynamic::ops::{db_source, program};
    use crate::dynamic::worker::DynamicWorker;
    use crate::views::xy_plot::{XyPlot, XyPlotPanelConfig, XyTraceConfig};
    use gpui::AppContext;
    use std::sync::Arc;

    let (db, _temp) = db_with_ids(&[
        ("rpm", ComponentId::new("rpm"), PrimType::F64, &[]),
        ("torque", ComponentId::new("torque"), PrimType::F64, &[]),
    ]);
    let db = Arc::new(db);
    let typed = "=torque * 2.0";

    let source_id = ComponentId::new("torque");
    let resolver = DbResolver::snapshot(&db);
    let compiled = Arc::new(program::Compiled::expression("torque * 2.0", &resolver).unwrap());
    let published = ComponentId::new(&expressions::component_name(program::field_id(
        compiled.system_hash(0, &[db_source::from_db_id(source_id)]),
        0,
    )));

    let saved = cx.update(|cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        expressions::Expressions::init(cx);
        DynamicWorker::init(cx);

        // What the wizard commits once the expression row has cascaded
        // through the element page: an ordinary component on X, the
        // expression's component on Y.
        let bound = expressions::bind(typed, &db, cx).expect("the expression compiles");
        let config = XyPlotPanelConfig {
            traces: vec![XyTraceConfig {
                x_component_id: ComponentId::new("rpm"),
                y_component_id: bound.id,
                ..Default::default()
            }],
            ..Default::default()
        };
        let plot = cx.new(|cx| XyPlot::from_config(config, db.clone(), cx));
        plot.read(cx).to_config(cx)
    });

    let trace = &saved.traces[0];
    assert_eq!(
        trace.y_expression.as_deref(),
        Some(typed),
        "the expression axis saves its text"
    );
    assert_eq!(trace.x_expression, None, "the ordinary axis is left alone");
    assert_eq!(trace.x_component_id, ComponentId::new("rpm"));

    // Reloading starts it again and binds what it publishes into.
    cx.update(|cx| {
        expressions::Expressions::init(cx);
        let plot = cx.new(|cx| XyPlot::from_config(saved, db.clone(), cx));
        let line_plot = plot.read(cx).line_plot().read(cx);
        let trace = line_plot.traces()[0].read(cx);
        assert_eq!(trace.y_component_id, published);
        assert_eq!(trace.x_component_id, ComponentId::new("rpm"));
        assert!(
            cx.global::<expressions::Expressions>().is_live(published),
            "reloading an expression axis starts it"
        );
    });
}

/// Compile an expression over `source` and report the component it will
/// publish into, without starting it.
fn expression_target(db: &std::sync::Arc<DB>, body: &str, source: ComponentId) -> ComponentId {
    use crate::dynamic::ops::{db_source, program};
    let resolver = DbResolver::snapshot(db);
    let compiled = std::sync::Arc::new(program::Compiled::expression(body, &resolver).unwrap());
    ComponentId::new(&expressions::component_name(program::field_id(
        compiled.system_hash(0, &[db_source::from_db_id(source)]),
        0,
    )))
}

/// Push one `f64` and wait for the expression's component to carry a sample
/// stamped `at`, returning its value.
///
/// The wait is a plain sleep rather than an async one: the expression's nodes
/// run on the stellarator worker thread, which the gpui test executor knows
/// nothing about and cannot be driven to park on.
fn feed_and_await(
    db: &std::sync::Arc<DB>,
    source: ComponentId,
    published: ComponentId,
    at: i64,
    value: f64,
) -> f64 {
    use metor_proto::types::Timestamp;
    let component = db.with_state(|s| s.get_component(source).cloned()).unwrap();
    component
        .push_buf(Timestamp(at), &value.to_le_bytes())
        .unwrap();
    let out = db
        .with_state(|s| s.get_component(published).cloned())
        .unwrap();
    for _ in 0..400 {
        if let Some(latest) = out.time_series.latest()
            && latest.timestamp() == Timestamp(at)
        {
            return f64::from_le_bytes(latest.data().try_into().unwrap());
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the expression never published at {at}");
}

/// A time-series trace bound to an expression, saved and reloaded.
///
/// The trace stores a component id, and an expression's component keeps its
/// history but not its computation across a restart — so an id alone comes
/// back as a frozen line with nothing writing to it. The last assertion is the
/// one that matters: after reloading, *new* data has to arrive, not just the
/// history that was already there.
#[gpui::test]
fn a_time_series_trace_bound_to_an_expression_restarts_on_reload(cx: &mut gpui::TestAppContext) {
    use crate::dynamic::worker::DynamicWorker;
    use crate::views::time_series::{PlotPanelConfig, TimeSeriesPlot, TraceConfig};
    use gpui::AppContext;
    use std::sync::Arc;

    let (db, _temp) = db_with_ids(&[("rpm", ComponentId::new("rpm"), PrimType::F64, &[])]);
    let db = Arc::new(db);
    let source = ComponentId::new("rpm");
    let typed = "=rpm * 2.0";
    let published = expression_target(&db, "rpm * 2.0", source);

    let (saved, plot) = cx.update(|cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        expressions::Expressions::init(cx);
        DynamicWorker::init(cx);

        let bound = expressions::bind(typed, &db, cx).expect("the expression compiles");
        assert_eq!(bound.id, published);
        let config = PlotPanelConfig {
            traces: vec![TraceConfig {
                component_id: bound.id,
                ..Default::default()
            }],
            ..Default::default()
        };
        let plot = cx.new(|cx| TimeSeriesPlot::from_config(config, db.clone(), cx));
        (plot.read(cx).to_config(cx), plot)
    });
    assert_eq!(
        saved.traces[0].expression.as_deref(),
        Some(typed),
        "a trace over an expression saves the text that made it"
    );

    // History accumulates while that session is running.
    assert_eq!(feed_and_await(&db, source, published, 100, 5.0), 10.0);

    // A new session: the old expression's handles go, and with them its
    // tasks. The component survives, holding what it already computed.
    drop(plot);
    cx.update(expressions::Expressions::init);

    let (reloaded, _plot) = cx.update(|cx| {
        let plot = cx.new(|cx| TimeSeriesPlot::from_config(saved, db.clone(), cx));
        let line_plot = plot.read(cx).line_plot().read(cx);
        let id = line_plot.traces()[0].read(cx).component_id;
        assert!(
            cx.global::<expressions::Expressions>().is_live(published),
            "reloading has to start the expression again"
        );
        (id, plot)
    });
    assert_eq!(reloaded, published, "and bind onto what it publishes into");

    // The point of all of it: the reloaded trace follows new data.
    assert_eq!(feed_and_await(&db, source, published, 200, 7.0), 14.0);
}

/// The same for a list plot, whose trace additionally carries a length.
#[gpui::test]
fn a_list_trace_bound_to_an_expression_restarts_on_reload(cx: &mut gpui::TestAppContext) {
    use crate::dynamic::worker::DynamicWorker;
    use crate::views::list_plot::{ListPlot, ListPlotPanelConfig, ListTraceConfig};
    use gpui::AppContext;
    use std::sync::Arc;

    let (db, _temp) = db_with_ids(&[("rpm", ComponentId::new("rpm"), PrimType::F64, &[])]);
    let db = Arc::new(db);
    let source = ComponentId::new("rpm");
    let typed = "=rpm * 3.0";
    let published = expression_target(&db, "rpm * 3.0", source);

    let (saved, plot) = cx.update(|cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        expressions::Expressions::init(cx);
        DynamicWorker::init(cx);

        let bound = expressions::bind(typed, &db, cx).expect("the expression compiles");
        let config = ListPlotPanelConfig {
            traces: vec![ListTraceConfig {
                component_id: bound.id,
                len: crate::views::list_plot::trace_picker::expression_len(&db, bound.id),
                ..Default::default()
            }],
            ..Default::default()
        };
        let plot = cx.new(|cx| ListPlot::from_config(config, db.clone(), cx));
        (plot.read(cx).to_config(cx), plot)
    });
    assert_eq!(saved.traces[0].expression.as_deref(), Some(typed));
    assert_eq!(saved.traces[0].len, 1, "a scalar expression is one point");

    assert_eq!(feed_and_await(&db, source, published, 100, 2.0), 6.0);

    drop(plot);
    cx.update(expressions::Expressions::init);

    let (reloaded, _plot) = cx.update(|cx| {
        let plot = cx.new(|cx| ListPlot::from_config(saved, db.clone(), cx));
        let line_plot = plot.read(cx).line_plot().read(cx);
        let id = line_plot.traces()[0].read(cx).component_id;
        assert!(
            cx.global::<expressions::Expressions>().is_live(published),
            "reloading has to start the expression again"
        );
        (id, plot)
    });
    assert_eq!(reloaded, published);

    assert_eq!(feed_and_await(&db, source, published, 200, 4.0), 12.0);
}

/// The user's report, verbatim in shape: a vector component contracted with
/// itself, parenthesised, through `resolve`'s exact path.
#[stellarator::test]
async fn a_self_dot_seeds_then_tails() {
    use crate::dynamic::ops::{db_source, program};
    use metor_proto::types::Timestamp;

    let path = "cube_sat.plant.body.omega_b";
    let (db, _temp) = db_with_ids(&[(path, ComponentId::new(path), PrimType::F64, &[3])]);
    let source_id = ComponentId::new(path);
    let source = db
        .with_state(|s| s.get_component(source_id).cloned())
        .unwrap();

    source
        .push_buf(Timestamp(100), &sample(&[1.0, 0.0, 0.0]))
        .unwrap();
    stellarator::sleep(std::time::Duration::from_millis(20)).await;

    let text = format!("({path} @ {path})");
    let resolver = DbResolver::snapshot(&db);
    let compiled = std::sync::Arc::new(program::Compiled::expression(&text, &resolver).unwrap());
    let inputs = &compiled.manifest.systems[0].inputs;
    assert_eq!(
        inputs.len(),
        1,
        "{:?}",
        inputs.iter().map(|p| &p.bindings).collect::<Vec<_>>()
    );
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
    let _published = expressions::publish(&db, &name, field, &text).unwrap();

    for step in 1..=4 {
        let v = 1.0 + step as f64;
        source
            .push_buf(Timestamp(100 + step), &sample(&[v, 0.0, 0.0]))
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
        "fault: {:?}; the expression stopped at {:?}",
        system.health.fault(),
        latest.timestamp()
    );
    assert_eq!(f64::from_le_bytes(latest.data().try_into().unwrap()), 25.0);
}

/// The user's report: `(omega_b @ omega_b)` typed into an existing plot showed
/// one sample and stopped. The picker resolves the expression and hands the
/// plot bare traces; once its own handle drops, nothing keeps the system
/// computing. The traces must carry the share.
#[gpui::test]
fn a_trace_added_from_the_picker_keeps_its_expression_alive(cx: &mut gpui::TestAppContext) {
    use crate::dynamic::worker::DynamicWorker;
    use crate::inspector::trace_picker::expression_traces;
    use std::sync::Arc;

    let path = "cube_sat.plant.body.omega_b";
    let (db, _temp) = db_with_ids(&[(path, ComponentId::new(path), PrimType::F64, &[3])]);
    let db = Arc::new(db);
    let text = format!("=({path} @ {path})");

    cx.update(|cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        expressions::Expressions::init(cx);
        DynamicWorker::init(cx);

        // What `ComputeRow::activate` does: resolve, hand out the id, drop.
        let traces = {
            let expression = expressions::resolve(&text, &db, cx).expect("compiles and starts");
            let id = expression.component_id();
            let basis: crate::inspector::trace_picker::ColorBasis = Arc::new(|_: &gpui::App| 0);
            expression_traces(&db, id, &text, &basis, cx)
        };
        let id = traces[0].component_id;
        assert!(
            expressions::running(id, cx).is_some(),
            "the trace's share must outlive the picker's"
        );
        drop(traces);
        assert!(
            expressions::running(id, cx).is_none(),
            "and removing the trace is what stops the expression"
        );
    });
}
