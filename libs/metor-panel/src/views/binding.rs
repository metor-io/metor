//! How instrument views attach to a component.
//!
//! Plots own their sampling; the small single-value views — traffic lights,
//! meters, gauges, state chips — all need the same three things instead, and
//! that overlap is what lives here. A view binds to *one element of one
//! component* ([`ElementRef`]), drains it with a task that outlives nothing
//! but the view itself, and reads back whatever the control system declared
//! about it.
//!
//! Three details are easy to get wrong alone and are therefore centralized:
//!
//! - **Seeding.** A fresh WAL reader only sees samples committed from now on.
//!   Without an explicit read of the last committed value, a meter placed on a
//!   slow component sits blank until the next sample — so the spawn helpers
//!   paint the seed before entering their loop.
//! - **Late binding.** A view can be placed, or restored from a layout, before
//!   its producer registers. [`spawn_meta_resolver`] waits on the DB's vtable
//!   generation and re-reads metadata once the schema appears.
//! - **Limits.** Warn/critical thresholds are already declared by the control
//!   system's alarm definitions, so an instrument should never be configured
//!   with them by hand. [`limit_marks`] and [`alarm_tint`] read the same store
//!   the plots read.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use gpui::{App, Context, Hsla, SharedString};
use metor_db::DB;
use metor_proto::types::{ComponentId, ComponentView, ElementValue, PrimType, Timestamp};
use metor_proto_wkt::Severity;
use smallvec::SmallVec;

use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

pub(crate) enum StreamUpdate<I, V> {
    Ready(I),
    Value(V),
    /// No sample has arrived for [`STALE_AFTER`]; the next `Value` ends it.
    /// Sent once per silence, not repeated.
    Stale,
}

/// How old a sample may be before it is stale — the age of the data by its
/// own stamp, not of its arrival, so a value the producer stopped refreshing
/// reads stale even while the link keeps delivering it.
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(3);

/// The producers' clock minus this machine's, as last observed when a
/// sample arrived. A simulated or skewed producer clock makes wall time the
/// wrong ruler: judged against it, a sim running ahead never ages and one
/// running behind is stale on arrival. Measuring age on the producer's own
/// timeline keeps "3 s" meaning 3 s of its time, and — because the offset is
/// frozen at the last arrival — a producer that stops still ages out on
/// this clock. One estimate serves every stream; producers on unrelated
/// clocks would each want their own.
static CLOCK_SKEW_MICROS: AtomicI64 = AtomicI64::new(0);

fn note_arrival(sample: Timestamp) {
    CLOCK_SKEW_MICROS.store(sample.0 - Timestamp::now().0, Ordering::Relaxed);
}

/// Now, on the producers' clock.
fn producer_now() -> Timestamp {
    Timestamp(Timestamp::now().0 + CLOCK_SKEW_MICROS.load(Ordering::Relaxed))
}

/// How long ago `sample` was taken on its producer's clock; zero for a stamp
/// still ahead of it.
fn sample_age(sample: Timestamp) -> Duration {
    let micros = producer_now().0.saturating_sub(sample.0).max(0);
    Duration::from_micros(micros as u64)
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
        loop {
            if db.with_state(|state| state.get_component(component_id).is_some()) {
                let meta = component_meta(&db, component_id);
                let _ = this.update(cx, |view, cx| apply(view, meta, cx));
                break;
            }
            db.vtable_gen.wait().await;
        }
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
/// The inspector writes a view's fields straight through facet's `Poke` and
/// then repaints; there is no hook to run a side effect. So a view whose
/// binding is editable compares the two at the top of its `render` and
/// respawns when they disagree — the same shape
/// [`Annunciator`](super::Annunciator) uses to recompile its glob.
pub(crate) fn rebound(want: ElementRef, bound: &mut Option<ElementRef>) -> bool {
    if *bound == Some(want) {
        return false;
    }
    *bound = Some(want);
    true
}

/// Deliver source initialization, the latest committed value, then updates.
pub(crate) fn spawn_seeded_stream<E, S, I, V, P, D, A>(
    db: Arc<DB>,
    source: S,
    cx: &mut Context<E>,
    prepare: P,
    apply: A,
) -> gpui::Task<()>
where
    E: 'static,
    S: ComponentStreamBuilder + Send + 'static,
    I: Send + 'static,
    V: Send + 'static,
    P: FnOnce(&DB, ComponentId) -> (I, D) + Send + 'static,
    D: for<'a> Fn(ComponentView<'a>) -> Option<V> + Send + 'static,
    A: Fn(&mut E, StreamUpdate<I, V>, &mut Context<E>) + Send + 'static,
{
    let component_id = source.component_id();
    cx.spawn(async move |this, cx| {
        let mut stream = source.into_stream(&db).await;
        let (initial, decode) = prepare(&db, component_id);
        if this
            .update(cx, |view, cx| apply(view, StreamUpdate::Ready(initial), cx))
            .is_err()
        {
            return;
        }

        // Every sample carries its stamp, so each one either is already
        // stale or says exactly how long until it will be. That remainder
        // arms one timer; once it fires the view is told and the task waits,
        // unarmed, for the sample that ends the silence. `None` means stale.
        let mut remaining: Option<Duration> = None;

        // The seed is committed history and may already be over the line.
        let seed = db.with_state(|state| {
            let component = state.get_component(component_id)?;
            let latest = component.time_series.latest()?;
            let (_size, view) = component.schema.parse_value(latest.data()).ok()?;
            Some((decode(view), sample_age(latest.timestamp())))
        });
        if let Some((seed, age)) = seed {
            remaining = STALE_AFTER.checked_sub(age);
            let stale = remaining.is_none();
            tracing::debug!(?component_id, ?age, stale, "seeded from history");
            let result = this.update(cx, |view, cx| {
                if let Some(seed) = seed {
                    apply(view, StreamUpdate::Value(seed), cx);
                }
                if stale {
                    apply(view, StreamUpdate::Stale, cx);
                }
            });
            if result.is_err() {
                return;
            }
        }

        loop {
            let next = async { Some(stream.next().await) };
            let arrived = match remaining {
                Some(wait) => {
                    let timeout = async {
                        cx.background_executor().timer(wait).await;
                        None
                    };
                    futures_lite::future::race(next, timeout).await
                }
                None => next.await,
            };
            let (update, stale) = match arrived {
                Some(sample) => {
                    // A source without stamps counts from arrival.
                    let stamp = sample.sample_time();
                    if let Some(stamp) = stamp {
                        note_arrival(stamp);
                    }
                    let age = stamp.map(sample_age).unwrap_or_default();
                    remaining = STALE_AFTER.checked_sub(age);
                    tracing::debug!(?component_id, ?age, stamped = stamp.is_some(), "sample");
                    let value = decode(sample.as_component_view()).map(StreamUpdate::Value);
                    (value, remaining.is_none())
                }
                None => {
                    remaining = None;
                    tracing::debug!(?component_id, "stale: timer elapsed");
                    (None, true)
                }
            };
            let result = this.update(cx, |view, cx| {
                if let Some(update) = update {
                    apply(view, update, cx);
                }
                if stale {
                    apply(view, StreamUpdate::Stale, cx);
                }
            });
            if result.is_err() {
                break;
            }
        }
    })
}

/// Spawn a task that drains a stream and forwards one element's value to
/// `apply` on the parent entity.
///
/// Seeds from the last committed sample before the first live one arrives.
/// The task exits when the entity is dropped (the `update` returns `Err`).
pub(crate) fn spawn_scalar_stream<E, F>(
    db: Arc<DB>,
    source: impl ComponentStreamBuilder + Send + 'static,
    element: usize,
    cx: &mut Context<E>,
    apply: F,
) -> gpui::Task<()>
where
    E: 'static,
    F: Fn(&mut E, f64, &mut Context<E>) + Send + 'static,
{
    spawn_seeded_stream(
        db,
        source,
        cx,
        move |_, _| {
            ((), move |view: ComponentView<'_>| {
                view.iter().nth(element).map(|value| value.as_f64())
            })
        },
        move |view, update, cx| {
            if let StreamUpdate::Value(value) = update {
                apply(view, value, cx);
            }
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
    F: Fn(&mut E, &[f64], &mut Context<E>) + Send + 'static,
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
        move |view, update, cx| {
            if let StreamUpdate::Value(values) = update {
                apply(view, &values, cx);
            }
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
    source: impl ComponentStreamBuilder + Send + 'static,
    cx: &mut Context<E>,
    apply: F,
) -> gpui::Task<()>
where
    E: 'static,
    F: Fn(&mut E, bool, Option<f64>, &mut Context<E>) + Send + 'static,
{
    spawn_seeded_stream(
        db,
        source,
        cx,
        |_, _| {
            ((), |view: ComponentView<'_>| {
                let leading = view.iter().next().map(|value| value.as_f64());
                Some((any_on(view.iter()), leading))
            })
        },
        move |view, update, cx| {
            if let StreamUpdate::Value((on, value)) = update {
                apply(view, on, value, cx);
            }
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
    fn an_unbound_instrument_binds_on_its_first_check() {
        let mut bound = None;
        assert!(rebound(ElementRef::new(ComponentId(1), 0), &mut bound));
        assert!(!rebound(ElementRef::new(ComponentId(1), 0), &mut bound));
    }
}

#[cfg(test)]
mod staleness_tests {
    use super::*;
    use metor_db::ComponentSchema;

    /// The seed judges age by the stamp the producer wrote, which the WAL
    /// carries through to the time series unchanged.
    #[stellarator::test]
    async fn the_seed_stamp_is_the_producers_not_the_receivers() {
        let temp = tempfile::tempdir().unwrap();
        let db = DB::create(temp.path().join("db")).unwrap();
        let id = ComponentId::new("dead.value");
        db.with_state_mut(|state| {
            state.insert_component(id, ComponentSchema::new(PrimType::F64, &[]), &db.path)
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
        assert!(sample_age(stamped) >= Duration::from_secs(9));
        assert!(STALE_AFTER.checked_sub(sample_age(stamped)).is_none());
    }
}
