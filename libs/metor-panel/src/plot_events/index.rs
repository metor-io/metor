//! Retained-event snapshots shared by timeline instances, independent of cursor time.
use super::*;
use gpui::{App, Global};
use std::{collections::HashMap, sync::Arc};

/// A stable retained record; spans can also be supplied by timeline clients.
#[derive(Clone, Debug)]
pub struct TimelineEvent {
    pub id: u64,
    pub event: PlotEvent,
    pub end: Option<Timestamp>,
    pub priority: u8,
}

/// Sorted retained history. Absence outside this snapshot never proves no events.
#[derive(Default)]
pub struct EventIndex {
    pub events: Vec<Arc<TimelineEvent>>,
    pub generation: u64,
    maximum: Vec<usize>,
    span_ends: Vec<i64>,
}
impl EventIndex {
    pub fn new(mut events: Vec<Arc<TimelineEvent>>, generation: u64) -> Self {
        events.sort_by_key(|e| (e.event.ts, e.id));
        let count = events.len();
        let mut maximum = vec![0; count * 2];
        for i in 0..count {
            maximum[count + i] = i;
        }
        for i in (1..count).rev() {
            let a = maximum[i * 2];
            let b = maximum[i * 2 + 1];
            maximum[i] = if events[a].priority >= events[b].priority {
                a
            } else {
                b
            };
        }
        let leaf_count = count.next_power_of_two();
        let mut span_ends = vec![i64::MIN; leaf_count * 2];
        for (i, event) in events.iter().enumerate() {
            span_ends[leaf_count + i] = event
                .end
                .filter(|end| *end > event.event.ts)
                .map_or(i64::MIN, |end| end.0);
        }
        for i in (1..leaf_count).rev() {
            span_ends[i] = span_ends[i * 2].max(span_ends[i * 2 + 1]);
        }
        Self {
            events,
            generation,
            maximum,
            span_ends,
        }
    }
    /// Find crossing spans without scanning every historical event or laying out unbounded overlaps.
    pub fn spans_in(&self, start: i64, end: i64, limit: usize) -> Vec<&Arc<TimelineEvent>> {
        if self.span_ends.is_empty() {
            return Vec::new();
        }
        let leaves = self.span_ends.len() / 2;
        let hi = self.events.partition_point(|e| e.event.ts.0 < end);
        let mut stack = vec![(1, 0, leaves)];
        let mut spans = Vec::new();
        while let Some((node, left, right)) = stack.pop() {
            if spans.len() == limit {
                break;
            }
            if left >= hi || self.span_ends[node] <= start {
                continue;
            }
            if right - left == 1 {
                spans.push(&self.events[left]);
            } else {
                let middle = (left + right) / 2;
                stack.push((node * 2 + 1, middle, right));
                stack.push((node * 2, left, middle));
            }
        }
        spans
    }
    /// Preserve the most severe member without rescanning dense bursts each paint.
    pub fn representative(&self, range: std::ops::Range<usize>) -> Option<&Arc<TimelineEvent>> {
        let n = self.events.len();
        let (mut left, mut right) = (range.start.min(n) + n, range.end.min(n) + n);
        let mut best: Option<usize> = None;
        let mut take = |node: usize| {
            let candidate = self.maximum[node];
            if best.is_none_or(|old| self.events[candidate].priority > self.events[old].priority) {
                best = Some(candidate);
            }
        };
        while left < right {
            if left % 2 == 1 {
                take(left);
                left += 1;
            }
            if right % 2 == 1 {
                right -= 1;
                take(right);
            }
            left /= 2;
            right /= 2;
        }
        best.map(|i| &self.events[i])
    }
    pub fn bounds(&self, start: i64, end: i64) -> std::ops::Range<usize> {
        self.events.partition_point(|e| e.event.ts.0 < start)
            ..self.events.partition_point(|e| e.event.ts.0 < end)
    }
    pub fn nearest(&self, time: i64, direction: i8) -> Option<&Arc<TimelineEvent>> {
        if direction < 0 {
            self.events
                .partition_point(|e| e.event.ts.0 < time)
                .checked_sub(1)
                .and_then(|i| self.events.get(i))
        } else {
            self.events
                .get(self.events.partition_point(|e| e.event.ts.0 <= time))
        }
    }
}

#[derive(Default)]
struct IndexCache(HashMap<EventKindKey, (Option<gpui::EntityId>, Arc<Theme>, Arc<EventIndex>)>);
impl Global for IndexCache {}

/// Build once per source generation across all consumers, never from the 500-flag API.
pub(crate) fn snapshot(source: &dyn EventSource, cx: &mut App) -> Arc<EventIndex> {
    let key = source.key();
    let generation = source.generation(cx);
    let owner = source.observe_target(cx).map(|e| e.entity_id());
    let theme = theme(cx);
    if let Some(cache) = cx.try_global::<IndexCache>()
        && let Some((old_owner, old_theme, frame)) = cache.0.get(&key)
        && *old_owner == owner
        && Arc::ptr_eq(old_theme, &theme)
        && frame.generation == generation
    {
        return frame.clone();
    }
    let previous = cx
        .try_global::<IndexCache>()
        .and_then(|cache| cache.0.get(&key))
        .filter(|(old_owner, old_theme, frame)| {
            *old_owner == owner && Arc::ptr_eq(old_theme, &theme) && frame.generation <= generation
        })
        .map(|(_, _, frame)| {
            frame
                .events
                .iter()
                .map(|e| (e.id, e.clone()))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let mut events = Vec::new();
    let mut push = |id: u64, priority, make: &mut dyn FnMut() -> PlotEvent| {
        events.push(previous.get(&id).cloned().unwrap_or_else(|| {
            Arc::new(TimelineEvent {
                id,
                event: make(),
                end: None,
                priority,
            })
        }));
    };
    match key {
        EventKindKey::Logs => {
            if let Some(store) = logs::try_global(cx) {
                for record in store.read(cx).state().history() {
                    let e = &record.event;
                    push(record.seq, e.level as u8, &mut || log_plot_event(e, &theme));
                }
            }
        }
        EventKindKey::Alarms => {
            if let Some(store) = alarms::try_global(cx) {
                let store = store.read(cx);
                let state = store.state();
                let first = state
                    .history_pushed()
                    .saturating_sub(state.history().len() as u64);
                for (i, e) in state.history().iter().enumerate() {
                    push(
                        first + i as u64,
                        e.severity.map_or(0, |s| s as u8 + 1),
                        &mut || alarm_plot_event(e, &theme),
                    );
                }
            }
        }
        EventKindKey::Sequences => {
            if let Some(store) = sequences::try_global(cx) {
                let store = store.read(cx);
                let state = store.state();
                let first = state
                    .history_pushed()
                    .saturating_sub(state.history().len() as u64);
                for (i, e) in state.history().iter().enumerate() {
                    push(first + i as u64, 0, &mut || sequence_plot_event(e, &theme));
                }
            }
        }
        EventKindKey::Msg(id) => {
            if let Some(store) = cx
                .try_global::<EventSourceRegistry>()
                .and_then(|r| r.msg_stores.get(&id))
            {
                let store = store.read(cx);
                let first = store.pushed.saturating_sub(store.history.len() as u64);
                let name = store.name();
                for (i, e) in store.history.iter().enumerate() {
                    push(first + i as u64, 0, &mut || {
                        msg_plot_event(e.ts, e.detail.clone(), &name, source.default_color(cx))
                    });
                }
            }
        }
    }
    let frame = Arc::new(EventIndex::new(events, generation));
    if cx.try_global::<IndexCache>().is_none() {
        cx.set_global(IndexCache::default());
    }
    let cache = cx.global_mut::<IndexCache>();
    if cache.0.len() >= 32 && !cache.0.contains_key(&key) {
        cache.0.clear();
    }
    cache.0.insert(key, (owner, theme, frame.clone()));
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overview_preserves_old_activity_and_severity_in_dense_bursts() {
        let events = (0..20_000)
            .rev()
            .map(|id| {
                Arc::new(TimelineEvent {
                    id,
                    event: PlotEvent {
                        ts: Timestamp(id as i64),
                        color: crate::theme::DARK.text_secondary,
                        label: "event".into(),
                        short: "event".into(),
                        detail: EventDetail::Raw(0),
                    },
                    end: None,
                    priority: if id == 5 { 10 } else { 0 },
                })
            })
            .collect();
        let index = EventIndex::new(events, 1);
        assert_eq!(index.bounds(0, 20_000).len(), 20_000);
        assert_eq!(index.bounds(0, 100).len(), 100);
        assert_eq!(index.representative(index.bounds(0, 20_000)).unwrap().id, 5);
        assert_eq!(index.nearest(5, -1).unwrap().id, 4);
        assert_eq!(index.nearest(5, 1).unwrap().id, 6);
        assert!(index.representative(0..0).is_none());
    }

    #[test]
    fn crossing_spans_remain_visible_and_queries_have_a_result_budget() {
        let events = (0..1000)
            .map(|id| {
                Arc::new(TimelineEvent {
                    id,
                    event: PlotEvent {
                        ts: Timestamp(id as i64),
                        color: crate::theme::DARK.text_secondary,
                        label: "span".into(),
                        short: "span".into(),
                        detail: EventDetail::Raw(0),
                    },
                    end: Some(Timestamp(10_000)),
                    priority: 0,
                })
            })
            .collect();
        let index = EventIndex::new(events, 1);
        assert_eq!(index.spans_in(5000, 6000, 100).len(), 100);
        assert!(index.spans_in(10_000, 20_000, 100).is_empty());
        assert!(index.spans_in(5000, 6000, 0).is_empty());
    }
}
