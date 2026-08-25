use std::time::Duration;

use metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_proto_wkt::{
    AlarmAck, AlarmCleared, AlarmDef, AlarmLimit, AlarmRaised, AlarmShelved, AlarmTarget,
    AlarmUnshelved, LimitKind, Severity,
};

use super::latch::TileState;
use super::{AlarmEventKind, AlarmState};
use crate::msg_ingest::{IngestSource, apply_backfill};

fn ts(n: i64) -> Timestamp {
    Timestamp(n)
}

fn raised(occurrence: u64, severity: Severity) -> AlarmRaised {
    AlarmRaised {
        def_id: "A".into(),
        occurrence,
        severity,
        value: Some(1.0),
        message: "fired".into(),
    }
}

fn def_with_target(element_index: Option<u64>) -> AlarmDef {
    AlarmDef {
        id: "A".into(),
        name: "Alarm A".into(),
        description: String::new(),
        target: Some(AlarmTarget {
            component_id: ComponentId::new("x.y"),
            element_index,
        }),
        limits: vec![AlarmLimit {
            kind: LimitKind::Upper,
            value: 10.0,
            severity: Severity::Warning,
            label: None,
        }],
        default_severity: Severity::Warning,
    }
}

/// A clear nobody acknowledged latches: the occurrence leaves the live set but stays
/// pending, so a transient alarm is still there to be dismissed.
#[test]
fn an_unacked_clear_latches() {
    let mut state = AlarmState::default();
    state.apply_raised(ts(1), raised(1, Severity::Warning));
    assert_eq!(state.active_sorted().len(), 1);
    assert_eq!(state.unacked_count(), 1);

    state.apply_cleared(
        ts(2),
        AlarmCleared {
            def_id: "A".into(),
            occurrence: 1,
        },
    );
    assert!(state.active_sorted().is_empty());
    assert_eq!(state.unacked_count(), 1);

    let pending = state.pending_sorted();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].state, TileState::ClearedUnacked);
    assert_eq!(pending[0].cleared_at, Some(ts(2)));
    assert_eq!(state.point(&"A".into()).state, TileState::ClearedUnacked);
}

/// Acking the latch is the only thing that retires it.
#[test]
fn an_ack_retires_a_latch() {
    let mut state = AlarmState::default();
    state.apply_raised(ts(1), raised(1, Severity::Warning));
    state.apply_cleared(
        ts(2),
        AlarmCleared {
            def_id: "A".into(),
            occurrence: 1,
        },
    );
    state.apply_ack(
        ts(3),
        AlarmAck {
            def_id: "A".into(),
            occurrence: 1,
            operator: "op".into(),
            note: None,
        },
    );
    assert_eq!(state.pending_count(), 0);
    assert_eq!(state.point(&"A".into()).state, TileState::Normal);
}

/// An occurrence the operator already dismissed leaves nothing behind when it clears.
#[test]
fn a_clear_after_an_ack_drops_the_occurrence() {
    let mut state = AlarmState::default();
    state.apply_raised(ts(1), raised(1, Severity::Warning));
    state.apply_ack(
        ts(2),
        AlarmAck {
            def_id: "A".into(),
            occurrence: 1,
            operator: "op".into(),
            note: None,
        },
    );
    state.apply_cleared(
        ts(3),
        AlarmCleared {
            def_id: "A".into(),
            occurrence: 1,
        },
    );
    assert_eq!(state.pending_count(), 0);
}

/// A chattering point replaces its own latch instead of growing the pending list.
#[test]
fn a_second_occurrence_replaces_the_latch() {
    let mut state = AlarmState::default();
    for occurrence in [1, 2] {
        state.apply_raised(
            ts(occurrence as i64 * 2),
            raised(occurrence, Severity::Warning),
        );
        state.apply_cleared(
            ts(occurrence as i64 * 2 + 1),
            AlarmCleared {
                def_id: "A".into(),
                occurrence,
            },
        );
    }
    let pending = state.pending_sorted();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].alarm.occurrence, 2);
}

/// The control system reuses the occurrence id when an alarm escalates, so an ack of
/// the milder band must not carry over to the worse one.
#[test]
fn an_escalation_clears_the_ack() {
    let mut state = AlarmState::default();
    state.apply_raised(ts(1), raised(1, Severity::Warning));
    state.apply_ack(
        ts(2),
        AlarmAck {
            def_id: "A".into(),
            occurrence: 1,
            operator: "op".into(),
            note: None,
        },
    );
    assert_eq!(state.unacked_count(), 0);

    state.apply_raised(ts(3), raised(1, Severity::Critical));
    assert!(!state.is_acked(1));
    assert_eq!(state.unacked_count(), 1);
}

/// A shelf hides the point from the pending list and the counts, and stops doing so the
/// moment it expires — evaluated at query time, with no removal task involved.
#[test]
fn a_shelf_expires_at_query_time() {
    let mut state = AlarmState::default();
    state.apply_raised(ts(1), raised(1, Severity::Warning));
    state.apply_shelved(
        ts(2),
        AlarmShelved {
            def_id: "A".into(),
            until: Timestamp::now() + Duration::from_secs(60),
            reason: Some("chattering".into()),
            operator: "op".into(),
        },
    );
    assert_eq!(state.pending_count(), 0);
    assert_eq!(state.counts_by_severity(), [0, 0, 0]);
    assert_eq!(state.shelves_sorted().len(), 1);
    assert_eq!(state.point(&"A".into()).state, TileState::Normal);
    // The occurrence is only hidden, never dropped.
    assert_eq!(state.active_sorted().len(), 1);

    state.apply_shelved(
        ts(3),
        AlarmShelved {
            def_id: "A".into(),
            until: Timestamp::now() - Duration::from_secs(1),
            reason: None,
            operator: "op".into(),
        },
    );
    assert_eq!(state.pending_count(), 1);
    assert!(state.shelves_sorted().is_empty());
}

/// Shelving must never mask an escalation: a raise above the severity the shelf was
/// taken at removes it.
#[test]
fn an_escalation_defeats_a_shelf() {
    let mut state = AlarmState::default();
    state.apply_raised(ts(1), raised(1, Severity::Warning));
    state.apply_shelved(
        ts(2),
        AlarmShelved {
            def_id: "A".into(),
            until: Timestamp::now() + Duration::from_secs(600),
            reason: None,
            operator: "op".into(),
        },
    );
    assert_eq!(state.pending_count(), 0);

    state.apply_raised(ts(3), raised(2, Severity::Critical));
    assert!(state.shelves_sorted().is_empty());
    assert_eq!(state.highest_pending_severity(), Some(Severity::Critical));
    assert!(
        state
            .history()
            .iter()
            .any(|event| event.kind == AlarmEventKind::Unshelved)
    );
}

/// An unshelve ends the suppression early and lands in history as the audit trail.
#[test]
fn an_unshelve_restores_the_point() {
    let mut state = AlarmState::default();
    state.apply_raised(ts(1), raised(1, Severity::Warning));
    state.apply_shelved(
        ts(2),
        AlarmShelved {
            def_id: "A".into(),
            until: Timestamp::now() + Duration::from_secs(600),
            reason: None,
            operator: "op".into(),
        },
    );
    state.apply_unshelved(
        ts(3),
        AlarmUnshelved {
            def_id: "A".into(),
            operator: "op".into(),
        },
    );
    assert_eq!(state.pending_count(), 1);
    let kinds: Vec<AlarmEventKind> = state.history().iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&AlarmEventKind::Shelved));
    assert!(kinds.contains(&AlarmEventKind::Unshelved));
}

#[test]
fn ack_marks_acked_only_while_active() {
    let mut state = AlarmState::default();
    state.apply_raised(ts(1), raised(1, Severity::Critical));
    state.apply_ack(
        ts(2),
        AlarmAck {
            def_id: "A".into(),
            occurrence: 1,
            operator: "op".into(),
            note: None,
        },
    );
    assert!(state.is_acked(1));
    assert_eq!(state.unacked_count(), 0);
    assert_eq!(state.active_sorted().len(), 1);

    // Clearing the occurrence also drops its ack so a re-raise starts unacked.
    state.apply_cleared(
        ts(3),
        AlarmCleared {
            def_id: "A".into(),
            occurrence: 1,
        },
    );
    assert!(!state.is_acked(1));
}

#[test]
fn ack_for_unknown_occurrence_is_ignored() {
    let mut state = AlarmState::default();
    state.apply_ack(
        ts(1),
        AlarmAck {
            def_id: "A".into(),
            occurrence: 99,
            operator: "op".into(),
            note: None,
        },
    );
    assert!(!state.is_acked(99));
    assert!(state.history().is_empty());
}

#[test]
fn clear_for_unknown_occurrence_is_ignored() {
    let mut state = AlarmState::default();
    state.apply_cleared(
        ts(1),
        AlarmCleared {
            def_id: "A".into(),
            occurrence: 99,
        },
    );
    assert!(state.history().is_empty());
}

#[test]
fn latest_def_wins() {
    let mut state = AlarmState::default();
    state.apply_def(def_with_target(Some(0)));
    let mut updated = def_with_target(Some(0));
    updated.name = "Renamed".into();
    state.apply_def(updated);
    assert_eq!(state.def(&"A".into()).unwrap().name, "Renamed");
}

#[test]
fn active_sorted_orders_by_severity_then_recency() {
    let mut state = AlarmState::default();
    state.apply_raised(ts(1), raised(1, Severity::Warning));
    state.apply_raised(ts(2), raised(2, Severity::Critical));
    state.apply_raised(ts(3), raised(3, Severity::Info));
    let sorted = state.active_sorted();
    assert_eq!(sorted[0].severity, Severity::Critical);
    assert_eq!(sorted[1].severity, Severity::Warning);
    assert_eq!(sorted[2].severity, Severity::Info);
}

#[test]
fn highest_pending_severity_and_counts() {
    let mut state = AlarmState::default();
    state.apply_raised(ts(1), raised(1, Severity::Warning));
    state.apply_raised(ts(2), raised(2, Severity::Critical));
    assert_eq!(state.highest_pending_severity(), Some(Severity::Critical));
    // [info, warning, critical]
    assert_eq!(state.counts_by_severity(), [0, 1, 1]);
}

#[test]
fn limits_and_active_severity_match_target() {
    let component = ComponentId::new("x.y");
    let mut state = AlarmState::default();
    state.apply_def(def_with_target(Some(0)));

    // Element 0 matches; element 1 does not (def is element-specific).
    assert_eq!(state.limits_for(component, 0).len(), 1);
    assert_eq!(state.limits_for(component, 1).len(), 0);

    state.apply_raised(ts(1), raised(1, Severity::Warning));
    assert_eq!(
        state.active_severity_for(component, 0),
        Some(Severity::Warning)
    );
    assert_eq!(state.active_severity_for(component, 1), None);
}

#[test]
fn whole_component_def_matches_any_element() {
    let component = ComponentId::new("x.y");
    let mut state = AlarmState::default();
    state.apply_def(def_with_target(None));
    assert_eq!(state.limits_for(component, 0).len(), 1);
    assert_eq!(state.limits_for(component, 7).len(), 1);
}

#[test]
fn history_records_event_kinds() {
    let mut state = AlarmState::default();
    state.apply_raised(ts(1), raised(1, Severity::Warning));
    state.apply_ack(
        ts(2),
        AlarmAck {
            def_id: "A".into(),
            occurrence: 1,
            operator: "op".into(),
            note: None,
        },
    );
    state.apply_cleared(
        ts(3),
        AlarmCleared {
            def_id: "A".into(),
            occurrence: 1,
        },
    );
    let kinds: Vec<AlarmEventKind> = state.history().iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            AlarmEventKind::Raised,
            AlarmEventKind::Acked,
            AlarmEventKind::Cleared,
        ]
    );
}

/// The store's ingest sources in declaration order, folding directly into a bare
/// [`AlarmState`]. `apply_backfill` is generic over its store type, so the same
/// (timestamp, source-index) merge the live store uses can be exercised without a
/// gpui `App` — the closures here mirror `AlarmStore::new`'s sources exactly.
fn alarm_sources() -> Vec<IngestSource<AlarmState>> {
    vec![
        IngestSource::new(AlarmDef::ID, |s: &mut AlarmState, _ts, def| {
            s.apply_def(def)
        }),
        IngestSource::new(AlarmRaised::ID, |s: &mut AlarmState, ts, r| {
            s.apply_raised(ts, r)
        }),
        IngestSource::new(AlarmCleared::ID, |s: &mut AlarmState, ts, c| {
            s.apply_cleared(ts, c)
        }),
        IngestSource::new(AlarmAck::ID, |s: &mut AlarmState, ts, a| s.apply_ack(ts, a)),
    ]
}

fn bytes<T: serde::Serialize>(value: &T) -> Vec<u8> {
    postcard::to_allocvec(value).unwrap()
}

/// A clear whose raise lives in a different log must still cancel it. The raise (ts 1)
/// and clear (ts 2) interleave across the raised/cleared logs; here the clear is listed
/// first, as a naive per-log replay (cleared log drained before the raised log) would
/// order it. `apply_backfill` sorts by (timestamp, source index), so the raise folds
/// before the clear and the occurrence ends cleared — not active forever.
#[test]
fn backfill_folds_cross_log_clear_after_its_raise() {
    let mut state = AlarmState::default();
    let mut sources = alarm_sources();

    let entries = vec![
        (
            ts(2),
            2,
            bytes(&AlarmCleared {
                def_id: "A".into(),
                occurrence: 1,
            }),
        ),
        (ts(1), 1, bytes(&raised(1, Severity::Warning))),
    ];
    apply_backfill(&mut state, &mut sources, entries);

    assert!(state.active_sorted().is_empty());
}

/// When a raise and its clear share one timestamp, the source declaration index breaks
/// the tie: raised (index 1) folds before cleared (index 2), so the occurrence still
/// clears. The clear is again listed first to prove the sort, not input order, decides.
#[test]
fn backfill_breaks_equal_timestamp_ties_by_source_index() {
    let mut state = AlarmState::default();
    let mut sources = alarm_sources();

    let entries = vec![
        (
            ts(5),
            2,
            bytes(&AlarmCleared {
                def_id: "A".into(),
                occurrence: 1,
            }),
        ),
        (ts(5), 1, bytes(&raised(1, Severity::Warning))),
    ];
    apply_backfill(&mut state, &mut sources, entries);

    assert!(state.active_sorted().is_empty());
}
