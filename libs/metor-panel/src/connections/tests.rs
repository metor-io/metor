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
