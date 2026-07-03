use metor_proto::types::Timestamp;
use metor_proto_wkt::{
    SequenceChannelEvent, SequenceChannelSpec, SequenceEventKind, SequenceRegistry,
    SequenceRunState,
};

use super::SequenceState;

fn ts(n: i64) -> Timestamp {
    Timestamp(n)
}

fn registry(channels: &[(&str, &[&str])]) -> SequenceRegistry {
    SequenceRegistry {
        channels: channels
            .iter()
            .map(|(name, available)| SequenceChannelSpec {
                name: (*name).to_string(),
                available: available.iter().map(|s| (*s).to_string()).collect(),
            })
            .collect(),
    }
}

fn event(channel: &str, kind: SequenceEventKind) -> SequenceChannelEvent {
    SequenceChannelEvent {
        channel: channel.to_string(),
        kind,
    }
}

#[test]
fn registry_declares_channels_in_order() {
    let mut state = SequenceState::default();
    state.apply_registry(
        ts(1),
        registry(&[("deploy", &["a", "b"]), ("attitude", &["c"])]),
    );

    let channels = state.channels_ordered();
    assert_eq!(channels.len(), 2);
    assert_eq!(channels[0].name.as_ref(), "deploy");
    assert_eq!(channels[1].name.as_ref(), "attitude");
    assert_eq!(channels[0].available.len(), 2);
    assert_eq!(channels[0].run_state, SequenceRunState::Idle);
    assert!(channels[0].loaded.is_none());
}

#[test]
fn lifecycle_folds_into_run_state() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[("deploy", &["solar"])]));

    state.apply_event(
        ts(2),
        event("deploy", SequenceEventKind::Loaded { name: "solar".into() }),
    );
    assert_eq!(
        state
            .channel("deploy")
            .unwrap()
            .loaded
            .as_ref()
            .map(|s| s.as_ref()),
        Some("solar")
    );

    state.apply_event(ts(3), event("deploy", SequenceEventKind::Started));
    assert_eq!(
        state.channel("deploy").unwrap().run_state,
        SequenceRunState::Running
    );

    state.apply_event(
        ts(4),
        event("deploy", SequenceEventKind::Progress { detail: "step 1".into() }),
    );
    assert_eq!(
        state
            .channel("deploy")
            .unwrap()
            .last_message
            .as_ref()
            .map(|s| s.as_ref()),
        Some("step 1")
    );
    assert_eq!(
        state.channel("deploy").unwrap().run_state,
        SequenceRunState::Running
    );

    state.apply_event(ts(5), event("deploy", SequenceEventKind::Stopped));
    assert_eq!(
        state.channel("deploy").unwrap().run_state,
        SequenceRunState::Stopped
    );
}

#[test]
fn registry_update_preserves_runtime_state() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[("deploy", &["solar"])]));
    state.apply_event(
        ts(2),
        event("deploy", SequenceEventKind::Loaded { name: "solar".into() }),
    );
    state.apply_event(ts(3), event("deploy", SequenceEventKind::Started));

    // Re-publishing the registry (e.g. after a reload) with the same channel must not wipe
    // the running sequence; only the available set is refreshed. (The name is the channel's
    // identity, so a renamed channel is a *different* channel, not an update.)
    state.apply_registry(ts(4), registry(&[("deploy", &["solar", "antenna"])]));
    let ch = state.channel("deploy").unwrap();
    assert_eq!(ch.available.len(), 2);
    assert_eq!(ch.loaded.as_ref().map(|s| s.as_ref()), Some("solar"));
    assert_eq!(ch.run_state, SequenceRunState::Running);
}

#[test]
fn registry_drops_removed_channels() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[("a", &[]), ("b", &[])]));
    assert_eq!(state.channel_count(), 2);

    state.apply_registry(ts(2), registry(&[("b", &[])]));
    assert_eq!(state.channel_count(), 1);
    assert!(state.channel("a").is_none());
    assert!(state.channel("b").is_some());
}

#[test]
fn events_for_undeclared_channels_are_ignored() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[("a", &[])]));
    state.apply_event(ts(2), event("nonesuch", SequenceEventKind::Started));
    assert!(state.channel("nonesuch").is_none());
    assert!(state.history().is_empty());
}

#[test]
fn history_caps_at_max() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[("a", &[])]));
    for i in 0..(super::MAX_HISTORY + 50) {
        state.apply_event(
            ts(i as i64 + 2),
            event("a", SequenceEventKind::Progress { detail: format!("{i}") }),
        );
    }
    assert_eq!(state.history().len(), super::MAX_HISTORY);
}

#[test]
fn reset_returns_completed_channel_to_idle() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[("deploy", &["solar"])]));
    state.apply_event(
        ts(2),
        event("deploy", SequenceEventKind::Loaded { name: "solar".into() }),
    );
    state.apply_event(ts(3), event("deploy", SequenceEventKind::Started));
    state.apply_event(ts(4), event("deploy", SequenceEventKind::Completed));
    assert_eq!(
        state.channel("deploy").unwrap().run_state,
        SequenceRunState::Completed
    );
    assert!(super::is_resettable(SequenceRunState::Completed));

    // The control system reports a reset as a fresh `Loaded`, returning the channel to idle.
    state.apply_event(
        ts(5),
        event("deploy", SequenceEventKind::Loaded { name: "solar".into() }),
    );
    let ch = state.channel("deploy").unwrap();
    assert_eq!(ch.run_state, SequenceRunState::Idle);
    assert_eq!(ch.loaded.as_ref().map(|s| s.as_ref()), Some("solar"));
    assert!(ch.last_message.is_none());
}

#[test]
fn is_resettable_only_in_terminal_safe_states() {
    use super::is_resettable;
    assert!(is_resettable(SequenceRunState::Completed));
    assert!(is_resettable(SequenceRunState::Aborted));
    assert!(!is_resettable(SequenceRunState::Idle));
    assert!(!is_resettable(SequenceRunState::Running));
    assert!(!is_resettable(SequenceRunState::Stopped));
    assert!(!is_resettable(SequenceRunState::Failed));
}

#[test]
fn count_in_state_tracks_run_states() {
    let mut state = SequenceState::default();
    state.apply_registry(ts(1), registry(&[("a", &[]), ("b", &[]), ("c", &[])]));
    state.apply_event(ts(2), event("a", SequenceEventKind::Started));
    state.apply_event(ts(3), event("b", SequenceEventKind::Started));
    state.apply_event(ts(4), event("c", SequenceEventKind::Failed { reason: "x".into() }));

    assert_eq!(state.count_in_state(SequenceRunState::Running), 2);
    assert_eq!(state.count_in_state(SequenceRunState::Failed), 1);
    assert_eq!(state.count_in_state(SequenceRunState::Idle), 0);
}
