use metor_proto::types::Timestamp;
use metor_proto_wkt::{
    ChannelId, SequenceChannelEvent, SequenceChannelSpec, SequenceEventKind, SequenceRegistry,
    SequenceRunState,
};

use super::SequenceState;

fn ts(n: i64) -> Timestamp {
    Timestamp(n)
}

fn registry(channels: &[(ChannelId, &str, &[&str])]) -> SequenceRegistry {
    SequenceRegistry {
        channels: channels
            .iter()
            .map(|(id, name, available)| SequenceChannelSpec {
                id: *id,
                name: (*name).to_string(),
                available: available.iter().map(|s| (*s).to_string()).collect(),
            })
            .collect(),
    }
}

fn event(channel_id: ChannelId, kind: SequenceEventKind) -> SequenceChannelEvent {
    SequenceChannelEvent { channel_id, kind }
}

#[test]
fn registry_declares_channels_in_order() {
    let mut state = SequenceState::default();
    state.apply_registry(
        ts(1),
        registry(&[(10, "Deploy", &["a", "b"]), (20, "Attitude", &["c"])]),
    );

    let channels = state.channels_ordered();
    assert_eq!(channels.len(), 2);
    assert_eq!(channels[0].id, 10);
    assert_eq!(channels[1].id, 20);
    assert_eq!(channels[0].available.len(), 2);
    assert_eq!(channels[0].run_state, SequenceRunState::Idle);
    assert!(channels[0].loaded.is_none());
}

#[test]
fn lifecycle_folds_into_run_state() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[(10, "Deploy", &["solar"])]));

    state.apply_event(
        ts(2),
        event(10, SequenceEventKind::Loaded { name: "solar".into() }),
    );
    assert_eq!(
        state.channel(10).unwrap().loaded.as_ref().map(|s| s.as_ref()),
        Some("solar")
    );

    state.apply_event(ts(3), event(10, SequenceEventKind::Started));
    assert_eq!(state.channel(10).unwrap().run_state, SequenceRunState::Running);

    state.apply_event(
        ts(4),
        event(10, SequenceEventKind::Progress { detail: "step 1".into() }),
    );
    assert_eq!(
        state
            .channel(10)
            .unwrap()
            .last_message
            .as_ref()
            .map(|s| s.as_ref()),
        Some("step 1")
    );
    assert_eq!(state.channel(10).unwrap().run_state, SequenceRunState::Running);

    state.apply_event(ts(5), event(10, SequenceEventKind::Stopped));
    assert_eq!(state.channel(10).unwrap().run_state, SequenceRunState::Stopped);
}

#[test]
fn registry_update_preserves_runtime_state() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[(10, "Deploy", &["solar"])]));
    state.apply_event(
        ts(2),
        event(10, SequenceEventKind::Loaded { name: "solar".into() }),
    );
    state.apply_event(ts(3), event(10, SequenceEventKind::Started));

    // Re-publishing the registry (e.g. after a reload) with the same channel must not wipe
    // the running sequence; only the name/available set are refreshed.
    state.apply_registry(ts(4), registry(&[(10, "Deploy v2", &["solar", "antenna"])]));
    let ch = state.channel(10).unwrap();
    assert_eq!(ch.name.as_ref(), "Deploy v2");
    assert_eq!(ch.available.len(), 2);
    assert_eq!(ch.loaded.as_ref().map(|s| s.as_ref()), Some("solar"));
    assert_eq!(ch.run_state, SequenceRunState::Running);
}

#[test]
fn registry_drops_removed_channels() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[(10, "A", &[]), (20, "B", &[])]));
    assert_eq!(state.channel_count(), 2);

    state.apply_registry(ts(2), registry(&[(20, "B", &[])]));
    assert_eq!(state.channel_count(), 1);
    assert!(state.channel(10).is_none());
    assert!(state.channel(20).is_some());
}

#[test]
fn events_for_undeclared_channels_are_ignored() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[(10, "A", &[])]));
    state.apply_event(ts(2), event(99, SequenceEventKind::Started));
    assert!(state.channel(99).is_none());
    assert!(state.history().is_empty());
}

#[test]
fn history_caps_at_max() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[(10, "A", &[])]));
    for i in 0..(super::MAX_HISTORY + 50) {
        state.apply_event(
            ts(i as i64 + 2),
            event(10, SequenceEventKind::Progress { detail: format!("{i}") }),
        );
    }
    assert_eq!(state.history().len(), super::MAX_HISTORY);
}

#[test]
fn count_in_state_tracks_run_states() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[(10, "A", &[]), (20, "B", &[]), (30, "C", &[])]));
    state.apply_event(ts(2), event(10, SequenceEventKind::Started));
    state.apply_event(ts(3), event(20, SequenceEventKind::Started));
    state.apply_event(ts(4), event(30, SequenceEventKind::Failed { reason: "x".into() }));

    assert_eq!(state.count_in_state(SequenceRunState::Running), 2);
    assert_eq!(state.count_in_state(SequenceRunState::Failed), 1);
    assert_eq!(state.count_in_state(SequenceRunState::Idle), 0);
}
