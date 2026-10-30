//! Instrument bindings: seed the last committed value, stream updates, and
//! resolve metadata for late-registering components. Alarm limits and colors
//! come from the shared alarm store.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, Context, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::{ComponentId, ComponentView, ElementValue, PrimType, Timestamp};
use metor_proto_wkt::Severity;
use smallvec::SmallVec;

pub(crate) enum StreamUpdate<I, V> {
    Ready(I),
    Value(V),
    /// No sample has arrived for [`STALE_AFTER`]; the next `Value` ends it.
    /// Sent once per silence, not repeated.
    Stale,
    /// No valid sample at the selected instant; discard the previous value.
    Unavailable,
}

/// How old a sample may be before it is stale — the age of the data by its
/// own stamp, not of its arrival, so a value the producer stopped refreshing
/// reads stale even while the link keeps delivering it.
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(3);

/// Per-stream freshness. Timestamps are absolute; future-dated samples age
/// from receipt, and repeated or regressing stamps never restart the timer.
pub(crate) struct Freshness {
    latest: Option<Timestamp>,
    advanced_at: Instant,
    age_at_advance: Duration,
}

impl Freshness {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            latest: None,
            advanced_at: now,
            age_at_advance: Duration::ZERO,
        }
    }

    pub(crate) fn observe(
        &mut self,
        stamp: Option<Timestamp>,
        wall: Timestamp,
        now: Instant,
    ) -> Duration {
        let age = stamp
            .map(|stamp| sample_age(stamp, wall))
            .unwrap_or_default();
        if stamp.is_none()
            || self
                .latest
                .is_none_or(|last| stamp.is_some_and(|ts| ts > last))
        {
            self.latest = stamp;
            self.advanced_at = now;
            self.age_at_advance = age;
        }
        age.max(
            self.age_at_advance
                .saturating_add(now.duration_since(self.advanced_at)),
        )
    }
}

fn sample_age(sample: Timestamp, now: Timestamp) -> Duration {
    Duration::from_micros(now.0.saturating_sub(sample.0).max(0) as u64)
}

/// The single number an instrument view displays: one element of one
/// component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ElementRef {
    pub component: ComponentId,
    pub element: usize,
}

impl ElementRef {
    pub fn new(component: ComponentId, element: usize) -> Self {
        Self { component, element }
    }
}

/// Per-component metadata instrument views read at construction and again
/// once the component registers.
pub(crate) struct ComponentMeta {
    pub name: SharedString,
    pub unit: SharedString,
    pub is_bool: bool,
    pub element_names: Vec<SharedString>,
}

/// Look up everything a view needs about a component in one DB pass.
///
/// Falls back to a debug-formatted id when the component has no metadata
/// yet, matching what the user sees in the picker.
pub(crate) fn component_meta(db: &DB, component_id: ComponentId) -> ComponentMeta {
    db.with_state(|state| {
        let meta = state.get_component_metadata(component_id);
        let name = meta
            .map(|m| SharedString::from(m.name.clone()))
            .unwrap_or_else(|| SharedString::from(format!("{:?}", component_id)));
        let unit = meta
            .and_then(|m| m.metadata.get("unit").cloned())
            .map(SharedString::from)
            .unwrap_or_default();
        let comp = state.get_component(component_id);
        let is_bool = comp
            .map(|c| c.schema.prim_type == PrimType::Bool)
            .unwrap_or(false);
        let element_names = comp
            .map(|c| {
                crate::inspector::trace_picker::element_names(c.schema.dim.as_slice())
                    .into_iter()
                    .map(SharedString::from)
                    .collect()
            })
            .unwrap_or_default();
        ComponentMeta {
            name,
            unit,
            is_bool,
            element_names,
        }
    })
}

/// Spawn a task that waits for `component_id` to register, then hands its
/// metadata to `apply` once.
///
/// A view placed (or restored) before its producer registers would otherwise
/// show the debug id forever and never learn its element names or unit.
pub(crate) fn spawn_meta_resolver<E, F>(
    db: Arc<DB>,
    component_id: ComponentId,
    cx: &mut Context<E>,
    apply: F,
) -> gpui::Task<()>
where
    E: 'static,
    F: Fn(&mut E, ComponentMeta, &mut Context<E>) + Send + 'static,
{
    cx.spawn(async move |this, cx| {
        crate::wait_for_component(&db, component_id).await;
        let meta = component_meta(&db, component_id);
        let _ = this.update(cx, |view, cx| apply(view, meta, cx));
    })
}

/// The component's registered name, or `None` when nothing has registered it.
///
/// Distinct from [`component_meta`]'s `name`, which falls back to a
/// debug-formatted id. That fallback is fine to *display* but must never be
/// persisted: a saved layout re-hashes the name to recover the id, so writing
/// `"ComponentId(123)"` would silently rebind the view somewhere else on the
/// next load.
pub(crate) fn component_name(db: &DB, component: ComponentId) -> Option<SharedString> {
    db.with_state(|state| {
        state
            .get_component_metadata(component)
            .map(|m| SharedString::from(m.name.clone()))
    })
}

/// Whether an instrument's editable binding has moved off what its stream
/// task is actually reading, recording the new target if so.
///
/// Inspector edit hooks call this synchronously; render-time checks also
/// catch programmatic changes to reflected fields.
pub(crate) fn rebound(want: ElementRef, bound: &mut Option<ElementRef>) -> bool {
    if *bound == Some(want) {
        return false;
    }
    *bound = Some(want);
    true
}

/// The stream half of an instrument's binding, held as one opaque field.
///
/// The editable half (`source`, element, label) stays on the instrument: the
/// inspector reflects a struct's own fields by index and can't flatten a
/// nested one.
pub(crate) struct LiveBinding<V> {
    bound: Option<ElementRef>,
    pub value: Option<V>,
    pub db: Arc<DB>,
    _registrations: gpui::Task<()>,
    stream: gpui::Task<()>,
    resolver: gpui::Task<()>,
}

impl<V> LiveBinding<V> {
    /// Bound to `at`; the caller starts the stream with [`restream`](Self::restream).
    pub(crate) fn new<E: 'static>(at: ElementRef, db: Arc<DB>, cx: &mut Context<E>) -> Self {
        Self {
            bound: Some(at),
            value: None,
            _registrations: crate::data_binding::watch_registrations(db.clone(), cx),
            db,
            stream: gpui::Task::ready(()),
            resolver: gpui::Task::ready(()),
        }
    }

    /// Bound to nothing, as an inspector "add" creates an entry.
    pub(crate) fn idle(db: Arc<DB>) -> Self {
        Self {
            bound: Some(ElementRef::new(
                crate::data_binding::Binding::default().id(),
                0,
            )),
            value: None,
            db,
            _registrations: gpui::Task::ready(()),
            stream: gpui::Task::ready(()),
            resolver: gpui::Task::ready(()),
        }
    }

    /// Resolve `source` and, when it points somewhere new, return the new
    /// target and its metadata. The previous value is dropped rather than
    /// left on screen under the new binding.
    pub(crate) fn moved(
        &mut self,
        source: &mut crate::data_binding::Binding,
        element: usize,
        cx: &mut App,
    ) -> Option<(ElementRef, ComponentMeta)> {
        source.resolve(&self.db, cx);
        let want = ElementRef::new(source.id(), element);
        if !rebound(want, &mut self.bound) {
            return None;
        }
        self.value = None;
        Some((want, component_meta(&self.db, want.component)))
    }

    pub(crate) fn restream(&mut self, stream: gpui::Task<()>, resolver: gpui::Task<()>) {
        self.stream = stream;
        self.resolver = resolver;
    }
}

/// A single-element instrument whose label and unit follow its component.
pub(crate) trait ScalarInstrument: 'static {
    /// Label, unit and stream, for tasks writing back into the view.
    fn scalar(&mut self) -> (&mut SharedString, &mut SharedString, &mut LiveBinding<f64>);
}

/// Stream `at` into `live`. Unless kept, label and unit are refreshed from
/// metadata once the component registers — a configured one is the
/// operator's, not the component's.
pub(crate) fn stream_scalar<E: ScalarInstrument>(
    live: &mut LiveBinding<f64>,
    at: ElementRef,
    keep_label: bool,
    keep_unit: bool,
    cx: &mut Context<E>,
) {
    let stream = spawn_scalar_stream(
        live.db.clone(),
        at.component,
        at.element,
        cx,
        |view: &mut E, value, cx| {
            view.scalar().2.value = value;
            cx.notify();
        },
    );
    let resolver = spawn_meta_resolver(
        live.db.clone(),
        at.component,
        cx,
        move |view: &mut E, meta, cx| {
            let (label, unit, _) = view.scalar();
            if !keep_label {
                *label = default_label(&meta.element_names, at.element, &meta.name);
            }
            if !keep_unit {
                *unit = meta.unit;
            }
            cx.notify();
        },
    );
    live.restream(stream, resolver);
}

/// The element's own name when the component has more than one, else the
/// component's — a single-element meter labelled `"[0]"` tells the operator
/// nothing.
pub(crate) fn default_label(
    element_names: &[SharedString],
    element: usize,
    component_name: &SharedString,
) -> SharedString {
    if element_names.len() > 1 {
        element_names
            .get(element)
            .cloned()
            .unwrap_or_else(|| component_name.clone())
    } else {
        component_name.clone()
    }
}

/// Deliver source initialization, the latest committed value, then updates.
pub(crate) fn spawn_seeded_stream<E, I, V, P, D, A>(
    db: Arc<DB>,
    component_id: ComponentId,
    cx: &mut Context<E>,
    prepare: P,
    apply: A,
) -> gpui::Task<()>
where
    E: 'static,
    I: Send + 'static,
    V: Send + 'static,
    P: FnOnce(&DB, ComponentId) -> (I, D) + Send + 'static,
    D: for<'a> Fn(ComponentView<'a>) -> Option<V> + Send + 'static,
    A: Fn(&mut E, StreamUpdate<I, V>, &mut Context<E>) + Send + 'static,
{
    spawn_stamped_stream(
        db,
        component_id,
        cx,
        move |db, id| {
            let (initial, decode) = prepare(db, id);
            (initial, move |_, view: ComponentView<'_>| decode(view))
        },
        apply,
    )
}

/// [`spawn_seeded_stream`], decoding each sample with its stamp: for a
/// component written only when something happens, where when it was written
/// is part of the value.
pub(crate) fn spawn_stamped_stream<E, I, V, P, D, A>(
    db: Arc<DB>,
    component_id: ComponentId,
    cx: &mut Context<E>,
    prepare: P,
    apply: A,
) -> gpui::Task<()>
where
    E: 'static,
    I: Send + 'static,
    V: Send + 'static,
    P: FnOnce(&DB, ComponentId) -> (I, D) + Send + 'static,
    D: for<'a> Fn(Timestamp, ComponentView<'a>) -> Option<V> + Send + 'static,
    A: Fn(&mut E, StreamUpdate<I, V>, &mut Context<E>) + Send + 'static,
{
    let reader = crate::temporal::samples::acquire(db.clone(), component_id, cx);
    cx.spawn(async move |this, cx| {
        let component = crate::wait_for_component(&db, component_id).await;
        let (initial, decode) = prepare(&db, component_id);
        let subscription = this.update(cx, |view, cx| {
            apply(view, StreamUpdate::Ready(initial), cx);
            let deliver = move |view: &mut E,
                                reader: &gpui::Entity<crate::temporal::samples::SelectedReader>,
                                cx: &mut Context<E>| {
                let selection = reader.read(cx).selection.clone();
                match selection.sample {
                    Some(sample) => {
                        if let Ok((_, value)) = component.schema.parse_value(&sample.bytes)
                            && let Some(value) = decode(sample.timestamp, value)
                        {
                            apply(view, StreamUpdate::Value(value), cx);
                            if selection.stale {
                                apply(view, StreamUpdate::Stale, cx);
                            }
                        } else {
                            apply(view, StreamUpdate::Unavailable, cx);
                        }
                    }
                    None => apply(view, StreamUpdate::Unavailable, cx),
                }
            };
            deliver(view, &reader, cx);
            cx.observe(&reader, move |view, reader, cx| deliver(view, &reader, cx))
        });
        // The task owns the shared-reader lease. GPUI observations
        // clear cached widget state in the same update cycle as a seek.
        std::future::pending::<()>().await;
        drop((subscription, reader));
    })
}

/// Spawn a task that drains a stream and forwards one element's value to
/// `apply` on the parent entity.
///
/// Seeds from the last committed sample before the first live one arrives.
/// The task exits when the entity is dropped (the `update` returns `Err`).
pub(crate) fn spawn_scalar_stream<E, F>(
    db: Arc<DB>,
    component_id: ComponentId,
    element: usize,
    cx: &mut Context<E>,
    apply: F,
) -> gpui::Task<()>
where
    E: 'static,
    F: Fn(&mut E, Option<f64>, &mut Context<E>) + Send + 'static,
{
    spawn_seeded_stream(
        db,
        component_id,
        cx,
        move |_, _| {
            ((), move |view: ComponentView<'_>| {
                view.iter().nth(element).map(|value| value.as_f64())
            })
        },
        move |view, update, cx| match update {
            StreamUpdate::Value(value) => apply(view, Some(value), cx),
            StreamUpdate::Unavailable => apply(view, None, cx),
            _ => {}
        },
    )
}

/// Spawn a task forwarding a component's leading `count` elements to `apply`.
///
/// The vector counterpart of [`spawn_scalar_stream`], for views bound to a
/// quaternion or a 3-vector rather than a single number. Samples shorter than
/// `count` are skipped rather than padded — a half-read attitude is worse
/// than a stale one.
pub(crate) fn spawn_elements_stream<E, F>(
    db: Arc<DB>,
    component: ComponentId,
    count: usize,
    cx: &mut Context<E>,
    apply: F,
) -> gpui::Task<()>
where
    E: 'static,
    F: Fn(&mut E, Option<&[f64]>, &mut Context<E>) + Send + 'static,
{
    spawn_seeded_stream(
        db,
        component,
        cx,
        move |_, _| {
            ((), move |view: ComponentView<'_>| {
                let values: SmallVec<[f64; 4]> = view
                    .iter()
                    .take(count)
                    .map(|value| value.as_f64())
                    .collect();
                (values.len() == count).then_some(values)
            })
        },
        move |view, update, cx| match update {
            StreamUpdate::Value(values) => apply(view, Some(&values), cx),
            StreamUpdate::Unavailable => apply(view, None, cx),
            _ => {}
        },
    )
}

/// Spawn a task that drains a stream and forwards the boolean [`any_on`],
/// paired with the sample's leading element as a number, to `apply`.
///
/// On/off views want the bit; an annunciator tile that shows a readout wants
/// the number behind it. Both come out of the same decode so a component is
/// never parsed twice for one sample.
///
/// The task exits when the entity is dropped (the `update` returns `Err`).
pub(crate) fn spawn_on_stream<E, F>(
    db: Arc<DB>,
    component_id: ComponentId,
    cx: &mut Context<E>,
    apply: F,
) -> gpui::Task<()>
where
    E: 'static,
    F: Fn(&mut E, Option<bool>, Option<f64>, &mut Context<E>) + Send + 'static,
{
    spawn_seeded_stream(
        db,
        component_id,
        cx,
        |_, _| {
            ((), |view: ComponentView<'_>| {
                let leading = view.iter().next().map(|value| value.as_f64());
                Some((any_on(view.iter()), leading))
            })
        },
        move |view, update, cx| match update {
            StreamUpdate::Value((on, value)) => apply(view, Some(on), value, cx),
            StreamUpdate::Unavailable => apply(view, None, None, cx),
            _ => {}
        },
    )
}

/// Returns `true` if any element of the iterator is "on".
///
/// `Bool` short-circuits to the element value; numeric elements are treated
/// as "on" when non-zero. Lets on/off views light up on numeric alarm or
/// status components without requiring callers to pre-classify the schema.
pub(crate) fn any_on(values: impl Iterator<Item = ElementValue>) -> bool {
    for v in values {
        if let ElementValue::Bool(b) = v {
            if b {
                return true;
            }
        } else if v.as_f64() != 0.0 {
            return true;
        }
    }
    false
}

/// Element 0 of a component read as a state code — a bool as 0/1 — for the
/// views that map codes to something (a toggle's segments, a state's
/// image). Held as one opaque field; [`follow`](Self::follow) every render.
#[derive(Default)]
pub(crate) struct CodeFeed {
    streaming: Option<ComponentId>,
    pub value: Option<f64>,
    pub stale: bool,
    pub is_bool: bool,
    _task: Option<gpui::Task<()>>,
}

impl CodeFeed {
    /// Stream `want`, restarting only when it changes; `None` stops.
    /// `feed` finds this feed on the view the task writes back into.
    pub(crate) fn follow<E: 'static>(
        &mut self,
        want: Option<ComponentId>,
        db: &Arc<DB>,
        cx: &mut Context<E>,
        feed: fn(&mut E) -> &mut CodeFeed,
    ) {
        if self.streaming == want {
            return;
        }
        *self = Self {
            streaming: want,
            _task: want.map(|id| spawn_code_feed(db.clone(), id, cx, feed)),
            ..Self::default()
        };
    }
}

fn spawn_code_feed<E: 'static>(
    db: Arc<DB>,
    id: ComponentId,
    cx: &mut Context<E>,
    feed: fn(&mut E) -> &mut CodeFeed,
) -> gpui::Task<()> {
    spawn_seeded_stream(
        db,
        id,
        cx,
        |db, id| {
            let is_bool = component_meta(db, id).is_bool;
            (is_bool, |view: ComponentView<'_>| {
                view.iter().next().map(|value| value.as_f64())
            })
        },
        move |view, update, cx| {
            let feed = feed(view);
            match update {
                StreamUpdate::Ready(is_bool) => feed.is_bool = is_bool,
                StreamUpdate::Value(value) => {
                    feed.value = Some(value);
                    feed.stale = false;
                }
                StreamUpdate::Stale => feed.stale = true,
                StreamUpdate::Unavailable => {
                    feed.value = None;
                    feed.stale = false;
                }
            }
            cx.notify();
        },
    )
}

/// Limit values declared for `at` by the control system's alarm definitions,
/// paired with the color of their severity.
///
/// Empty when the alarm store was never initialized (tests) or the element
/// has no declared limits — an instrument then draws its scale unannotated.
pub(crate) fn limit_marks(at: ElementRef, cx: &App) -> SmallVec<[(f64, Hsla); 4]> {
    let Some(store) = crate::alarms::try_global(cx) else {
        return SmallVec::new();
    };
    let theme = crate::theme::theme(cx);
    store
        .read(cx)
        .state()
        .limits_for(at.component, at.element)
        .into_iter()
        .map(|limit| {
            let idx = crate::alarms::severity_index(limit.severity);
            (limit.value, theme.alarm_color(idx))
        })
        .collect()
}

/// Severity of the worst alarm currently raised against `at`, if any.
pub(crate) fn active_severity(at: ElementRef, cx: &App) -> Option<Severity> {
    if !crate::temporal::is_live(cx) {
        return None;
    }
    crate::alarms::try_global(cx)?
        .read(cx)
        .state()
        .active_severity_for(at.component, at.element)
}

/// Wash color for an instrument whose element currently has an active alarm.
pub(crate) fn alarm_tint(at: ElementRef, cx: &App) -> Option<Hsla> {
    let severity = active_severity(at, cx)?;
    Some(crate::theme::theme(cx).alarm_tint(crate::alarms::severity_index(severity)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_on_bool() {
        assert!(any_on([ElementValue::Bool(true)].into_iter()));
        assert!(!any_on([ElementValue::Bool(false)].into_iter()));
    }

    #[test]
    fn any_on_numeric() {
        assert!(any_on([ElementValue::I32(5)].into_iter()));
        assert!(!any_on([ElementValue::I32(0)].into_iter()));
        assert!(any_on(
            [ElementValue::F32(0.0), ElementValue::F32(1.5)].into_iter()
        ));
        assert!(!any_on([ElementValue::F32(0.0)].into_iter()));
    }

    #[test]
    fn any_on_empty() {
        assert!(!any_on(Vec::<ElementValue>::new().into_iter()));
    }

    /// The rebind check runs on every frame, so "unchanged" has to be the
    /// cheap, silent path — a helper that reported a rebind each time would
    /// respawn the stream task 120 times a second.
    #[test]
    fn an_unchanged_binding_never_reports_a_rebind() {
        let at = ElementRef::new(ComponentId(7), 1);
        let mut bound = Some(at);
        assert!(!rebound(at, &mut bound));
        assert!(!rebound(at, &mut bound));
    }

    #[test]
    fn a_changed_binding_reports_once_and_then_settles() {
        let mut bound = Some(ElementRef::new(ComponentId(7), 1));

        // A different component rebinds.
        let moved = ElementRef::new(ComponentId(9), 1);
        assert!(rebound(moved, &mut bound));
        assert!(!rebound(moved, &mut bound));

        // So does a different element of the same component: an instrument
        // pointed at gyro x is not the one pointed at gyro y.
        let other_element = ElementRef::new(ComponentId(9), 2);
        assert!(rebound(other_element, &mut bound));
        assert!(!rebound(other_element, &mut bound));
    }

    #[test]
    fn label_prefers_the_element_name_only_when_there_are_several() {
        let comp = SharedString::from("plant.wheels");
        let many: Vec<SharedString> = vec!["x".into(), "y".into(), "z".into()];
        assert_eq!(default_label(&many, 1, &comp), SharedString::from("y"));
        let one: Vec<SharedString> = vec!["[0]".into()];
        assert_eq!(default_label(&one, 0, &comp), comp);
        assert_eq!(default_label(&many, 9, &comp), comp);
    }

    #[test]
    fn an_unbound_instrument_binds_on_its_first_check() {
        let mut bound = None;
        assert!(rebound(ElementRef::new(ComponentId(1), 0), &mut bound));
        assert!(!rebound(ElementRef::new(ComponentId(1), 0), &mut bound));
    }
}

#[cfg(test)]
mod staleness_tests {
    use super::*;

    #[test]
    fn delayed_and_repeated_samples_do_not_refresh_their_age() {
        let now = Instant::now();
        let mut clock = Freshness::new(now);
        assert_eq!(
            clock.observe(Some(Timestamp(0)), Timestamp(10_000_000), now),
            Duration::from_secs(10)
        );
        assert_eq!(
            clock.observe(
                Some(Timestamp(0)),
                Timestamp(11_000_000),
                now + Duration::from_secs(1)
            ),
            Duration::from_secs(11)
        );
        assert_eq!(
            clock.observe(
                Some(Timestamp(11_000_000)),
                Timestamp(11_000_000),
                now + Duration::from_secs(1)
            ),
            Duration::ZERO
        );
    }

    #[test]
    fn future_stamps_age_from_receipt_and_clocks_are_independent() {
        let now = Instant::now();
        let mut future = Freshness::new(now);
        let mut ordinary = Freshness::new(now);
        assert_eq!(
            future.observe(Some(Timestamp(i64::MAX)), Timestamp(0), now),
            Duration::ZERO
        );
        assert_eq!(
            ordinary.observe(Some(Timestamp(0)), Timestamp(5_000_000), now),
            Duration::from_secs(5)
        );
        assert_eq!(
            future.observe(
                Some(Timestamp(i64::MAX)),
                Timestamp(5_000_000),
                now + Duration::from_secs(5)
            ),
            Duration::from_secs(5)
        );
        assert_eq!(
            ordinary.observe(None, Timestamp(5_000_000), now + Duration::from_secs(5)),
            Duration::ZERO
        );
    }

    use metor_db::ComponentSchema;

    /// The seed judges age by the stamp the producer wrote, which the WAL
    /// carries through to the time series unchanged.
    #[stellarator::test]
    async fn the_seed_stamp_is_the_producers_not_the_receivers() {
        let temp = tempfile::tempdir().unwrap();
        let db = DB::create(temp.path().join("db")).unwrap();
        let id = ComponentId::new("dead.value");
        db.with_state_mut(|state| {
            state.insert_component(id, ComponentSchema::new(PrimType::F64, &[][..]), &db.path)
        })
        .unwrap();
        let component = db
            .with_state(|state| state.get_component(id).cloned())
            .unwrap();
        let stamped = Timestamp(Timestamp::now().0 - 10_000_000);
        component.push_buf(stamped, &1.5f64.to_le_bytes()).unwrap();
        for _ in 0..200 {
            if component.time_series.latest().is_some() {
                break;
            }
            stellarator::sleep(Duration::from_millis(5)).await;
        }
        let latest = component.time_series.latest().expect("persisted");
        assert_eq!(latest.timestamp(), stamped);
        assert!(sample_age(stamped, Timestamp::now()) >= Duration::from_secs(9));
        assert!(
            STALE_AFTER
                .checked_sub(sample_age(stamped, Timestamp::now()))
                .is_none()
        );
    }
}
