use super::*;

fn target(id: &str, name: &str) -> ConnectionTarget {
    ConnectionTarget::custom(
        id.to_string(),
        name.to_string(),
        "",
        |_ctx: ConnectContext| Connected::default(),
    )
}

#[test]
fn upsert_dedups_by_id_and_updates_fields() {
    let mut state = ConnectionsState::default();
    state.upsert_target(target("a", "Alpha"));
    state.upsert_target(target("b", "Beta"));
    state.upsert_target(target("a", "Alpha (renamed)"));

    let names: Vec<&str> = state.targets().iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, vec!["Alpha (renamed)", "Beta"]);
}

#[test]
fn remove_target_by_id() {
    let mut state = ConnectionsState::default();
    state.upsert_target(target("a", "Alpha"));
    state.upsert_target(target("b", "Beta"));
    state.remove_target(&TargetId("a".into()));

    let ids: Vec<&str> = state.targets().iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, vec!["b"]);
}

#[test]
fn status_generation_bumps_only_on_change() {
    let status = StatusHandle::default();
    assert_eq!(status.generation(), 0);
    assert_eq!(status.get(), ConnectionStatus::Connecting);

    status.set(ConnectionStatus::Connecting);
    assert_eq!(status.generation(), 0);

    status.set(ConnectionStatus::Connected);
    assert_eq!(status.generation(), 1);
    assert_eq!(status.get(), ConnectionStatus::Connected);

    status.set(ConnectionStatus::Connected);
    assert_eq!(status.generation(), 1);

    status.set(ConnectionStatus::Reconnecting);
    assert_eq!(status.generation(), 2);
}

#[test]
fn tcp_target_id_derives_from_addr() {
    let target = ConnectionTarget::tcp("Vehicle", "127.0.0.1:2240".parse().unwrap());
    assert_eq!(target.id.as_str(), "tcp:127.0.0.1:2240");
    assert_eq!(target.detail.as_ref(), "127.0.0.1:2240");
}

#[test]
fn sections_order_favorites_recents_discovered() {
    let mut state = ConnectionsState::default();
    state.upsert_target(target("a", "Alpha"));
    state.upsert_target(target("b", "Beta"));
    state.upsert_target(target("c", "Gamma"));
    state.record_connected(&target("b", "Beta"), 100);
    state.toggle_favorite(&TargetId("c".into()));

    let sections = state.sections();
    let names = |entries: &[PickerEntry]| -> Vec<String> {
        entries.iter().map(|e| e.name.to_string()).collect()
    };
    assert_eq!(names(&sections.favorites), vec!["Gamma"]);
    assert_eq!(names(&sections.recents), vec!["Beta"]);
    assert_eq!(names(&sections.discovered), vec!["Alpha"]);
    // Favorites and recents don't repeat in discovered.
    assert!(sections.favorites[0].target.is_some());
}

#[test]
fn recents_move_to_front_and_cap() {
    let mut state = ConnectionsState::default();
    for i in 0..20 {
        state.record_connected(&target(&format!("t{i}"), "T"), i);
    }
    state.record_connected(&target("t3", "T"), 99);

    let index = state.index();
    assert_eq!(index.recents.len(), persist::RECENTS_CAP);
    assert_eq!(index.recents[0].id, "t3");
    assert_eq!(index.recents[0].last_connected_unix, 99);
}

#[test]
fn favorite_toggle_round_trips() {
    let mut state = ConnectionsState::default();
    let id = TargetId("a".into());
    state.toggle_favorite(&id);
    assert!(state.is_favorite(&id));
    state.toggle_favorite(&id);
    assert!(!state.is_favorite(&id));
}

#[test]
fn tcp_recent_rematerializes_without_registry() {
    let mut state = ConnectionsState::default();
    let tcp = ConnectionTarget::tcp("Vehicle", "10.0.0.7:2240".parse().unwrap());
    state.record_connected(&tcp, 1);
    // Registry is empty (fresh session); the recent still yields a target.
    let sections = state.sections();
    assert_eq!(sections.recents.len(), 1);
    let entry = &sections.recents[0];
    assert!(entry.target.is_some());
    assert_eq!(entry.target.as_ref().unwrap().id, tcp.id);

    // A custom-backend recent can't come back without its discoverer.
    let mut state = ConnectionsState::default();
    state.record_connected(&target("sim", "Sim"), 1);
    let sections = state.sections();
    assert!(sections.recents[0].target.is_none());
}

#[test]
fn custom_resolver_revives_addressed_recents() {
    let mut state = ConnectionsState::default();
    state.set_resolver(AddressResolver {
        placeholder: "vehicle-url".into(),
        resolve: std::sync::Arc::new(|input| {
            let serial = input
                .strip_prefix("veh://")
                .ok_or_else(|| gpui::SharedString::new_static("expected veh://<serial>"))?;
            Ok(target(&format!("veh-{serial}"), serial).with_address(input.to_string()))
        }),
    });
    // A protocol-addressed target connects once, then its discoverer is
    // gone after restart — the resolver brings it back, and the recent's
    // display name wins over the resolver's generic one.
    let veh = (state.resolver().resolve)("veh://0042").unwrap();
    state.record_connected(&veh, 7);
    state.remove_target(&veh.id);

    let sections = state.sections();
    let entry = &sections.recents[0];
    let revived = entry.target.as_ref().expect("resolver revives the recent");
    assert_eq!(revived.id.as_str(), "veh-0042");
    assert_eq!(revived.name.as_ref(), "0042");

    // Garbage input surfaces the resolver's own error message.
    let Err(err) = (state.resolver().resolve)("127.0.0.1:2240") else {
        panic!("resolver accepted non-veh input");
    };
    assert_eq!(err.as_ref(), "expected veh://<serial>");
}

#[test]
fn index_round_trips_and_tolerates_empty_object() {
    let mut state = ConnectionsState::default();
    state.toggle_favorite(&TargetId("a/b c".into()));
    state.record_connected(
        &ConnectionTarget::tcp("V", "127.0.0.1:1".parse().unwrap()),
        42,
    );
    let json = facet_json::to_string(&state.index()).unwrap();
    let parsed: persist::ConnectionsIndex = facet_json::from_str(&json).unwrap();
    assert_eq!(parsed.favorites, vec!["a/b c".to_string()]);
    assert_eq!(parsed.recents[0].address.as_deref(), Some("127.0.0.1:1"));
    assert_eq!(parsed.recents[0].last_connected_unix, 42);

    let empty: persist::ConnectionsIndex = facet_json::from_str("{}").unwrap();
    assert!(empty.favorites.is_empty() && empty.recents.is_empty());
}

// ---------------------------------------------------------------------------
// Connection options: spec resolution, override folding, and index round-trip.
// ---------------------------------------------------------------------------

fn knobs() -> Vec<ConnectionOption> {
    vec![
        ConnectionOption::toggle("commanding", "Allow commanding", true),
        ConnectionOption::choice("rate", "Telemetry", ["Full", "Decimated"], 1),
        ConnectionOption::number("depth", "Buffer depth", 4096.0, 0.0, 65536.0),
    ]
}

#[test]
fn options_resolve_to_spec_defaults_then_overrides() {
    let mut state = ConnectionsState::default();
    let sim = target("sim", "Sim").with_options(knobs());

    let defaults = state.options_for(&sim);
    assert!(defaults.bool("commanding"));
    assert_eq!(defaults.choice("rate"), "Decimated");
    assert_eq!(defaults.number("depth"), 4096.0);

    state.set_option(&sim.id, "commanding", OptionValue::Bool(false));
    state.set_option(&sim.id, "rate", OptionValue::Choice("Full".into()));
    let set = state.options_for(&sim);
    assert!(!set.bool("commanding"));
    assert_eq!(set.choice("rate"), "Full");
    // Untouched knobs still come from the spec.
    assert_eq!(set.number("depth"), 4096.0);

    // Overrides are per-target: a sibling declaring the same keys is
    // unaffected.
    let bench = target("bench", "Bench").with_options(knobs());
    assert!(state.options_for(&bench).bool("commanding"));
}

#[test]
fn panel_wide_default_covers_only_undeclared_targets() {
    let mut state = ConnectionsState::default();
    state.set_default_options(
        vec![ConnectionOption::toggle(
            "commanding",
            "Allow commanding",
            false,
        )]
        .into(),
    );

    // A discovered target declares nothing, so the panel-wide set is its
    // whole configuration surface.
    let discovered = ConnectionTarget::tcp("Vehicle", "10.0.0.7:2240".parse().unwrap());
    assert_eq!(state.spec_for(&discovered).len(), 1);
    assert!(!state.options_for(&discovered).bool("commanding"));

    // A target with its own declaration replaces the fallback rather than
    // merging with it.
    let sim = target("sim", "Sim").with_options(knobs());
    assert_eq!(state.spec_for(&sim).len(), 3);
    assert!(state.options_for(&sim).bool("commanding"));
}

#[test]
fn option_index_round_trips_and_retypes_against_the_spec() {
    let mut state = ConnectionsState::default();
    let sim = target("sim", "Sim").with_options(knobs());
    state.set_option(&sim.id, "commanding", OptionValue::Bool(false));
    state.set_option(&sim.id, "depth", OptionValue::Number(128.0));

    let json = facet_json::to_string(&state.index()).unwrap();
    let parsed: persist::ConnectionsIndex = facet_json::from_str(&json).unwrap();
    let mut restored = ConnectionsState::default();
    restored.load_index(parsed);

    let values = restored.options_for(&sim);
    assert!(!values.bool("commanding"));
    assert_eq!(values.number("depth"), 128.0);
    assert_eq!(values.choice("rate"), "Decimated");

    // A knob whose declaration changed under a saved answer falls back to
    // its new default instead of poisoning the file: `commanding` is now a
    // choice, and `depth`'s saved number no longer parses as one.
    let rebuilt = target("sim", "Sim").with_options(vec![
        ConnectionOption::choice("commanding", "Commanding", ["Off", "On"], 1),
        ConnectionOption::text("depth", "Buffer", "n", "auto"),
    ]);
    let values = restored.options_for(&rebuilt);
    assert_eq!(values.choice("commanding"), "On");
    // Text accepts any string, so the saved number survives as one.
    assert_eq!(values.text("depth"), "128");

    // A target that never had options contributes nothing to the file.
    let empty: persist::ConnectionsIndex = facet_json::from_str("{}").unwrap();
    assert!(empty.options.is_empty());
}

#[test]
fn sanitize_id_is_safe_and_collision_proof() {
    let a = persist::sanitize_id("tcp:127.0.0.1:2240");
    assert!(!a.contains(':') && !a.contains('/'));
    // Two ids that sanitize to the same character string stay distinct
    // through the hash suffix.
    let b = persist::sanitize_id("a/b");
    let c = persist::sanitize_id("a:b");
    assert_ne!(b, c);
    // Unicode collapses without panicking.
    let d = persist::sanitize_id("véhicule-1");
    assert!(d.is_ascii());
}

// ---------------------------------------------------------------------------
// The default resolver's protocol grammar: bare = discover, schemes pin.
// ---------------------------------------------------------------------------

#[test]
fn default_resolver_grammar_parses_all_three_forms() {
    let resolver = AddressResolver::default();
    // All three forms address the same system, so they share one id — a
    // layout belongs to the address, not the transport mode.
    let bare = (resolver.resolve)("127.0.0.1:2240").unwrap();
    let db = (resolver.resolve)("db://127.0.0.1:2240").unwrap();
    let fsw = (resolver.resolve)("fsw://127.0.0.1:2240").unwrap();
    assert_eq!(bare.id.as_str(), "tcp:127.0.0.1:2240");
    assert_eq!(db.id, bare.id);
    assert_eq!(fsw.id, bare.id);
    // The pinned forms keep their scheme in the persistable address, so a
    // recent re-materializes with the pin intact.
    assert_eq!(
        bare.address.as_ref().map(|a| a.as_ref()),
        Some("127.0.0.1:2240")
    );
    assert_eq!(
        db.address.as_ref().map(|a| a.as_ref()),
        Some("db://127.0.0.1:2240")
    );
    assert_eq!(
        fsw.address.as_ref().map(|a| a.as_ref()),
        Some("fsw://127.0.0.1:2240")
    );

    let Err(err) = (resolver.resolve)("carrier://127.0.0.1:2240") else {
        panic!("junk scheme resolved")
    };
    assert!(
        err.contains("db://"),
        "the error teaches the grammar: {err}"
    );
    let Err(err) = (resolver.resolve)("not-an-addr") else {
        panic!("junk address resolved")
    };
    assert!(err.contains("host:port"), "{err}");
}

#[test]
fn schemed_recents_rematerialize_and_replace_by_id() {
    let mut state = ConnectionsState::default();
    let fsw = (state.resolver().resolve)("fsw://10.0.0.5:2240").unwrap();
    state.record_connected(&fsw, 7);

    // A fresh session (empty registry): the recent still yields a
    // connectable target through the default resolver.
    let sections = state.sections();
    let entry = &sections.recents[0];
    assert!(entry.target.is_some(), "fsw recent re-materialized");

    // Connecting to the same address in another mode replaces the recent
    // (retain-by-id), never duplicating the row or forking the layout.
    let bare = (state.resolver().resolve)("10.0.0.5:2240").unwrap();
    state.record_connected(&bare, 9);
    let index = state.index();
    assert_eq!(index.recents.len(), 1);
    assert_eq!(index.recents[0].address.as_deref(), Some("10.0.0.5:2240"));
}
