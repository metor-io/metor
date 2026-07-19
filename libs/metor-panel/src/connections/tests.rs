use super::*;

fn target(id: &str, name: &str) -> ConnectionTarget {
    ConnectionTarget::custom(id.to_string(), name.to_string(), "", |_ctx: ConnectContext| {
        Connected::default()
    })
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
    assert_eq!(parsed.recents[0].tcp_addr.as_deref(), Some("127.0.0.1:1"));
    assert_eq!(parsed.recents[0].last_connected_unix, 42);

    let empty: persist::ConnectionsIndex = facet_json::from_str("{}").unwrap();
    assert!(empty.favorites.is_empty() && empty.recents.is_empty());
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
