//! Owned data inputs shared by instruments, traces and model bindings.
//!
//! A binding is replaced as a value: its source description and computation
//! owner can never be committed independently. Clones share computation, not
//! editable state. Component ids are a resolved DB address, not saved syntax.

use crate::dynamic::expressions::{self, Expression};
use gpui::App;
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BindingSpec {
    Unbound,
    Component {
        id: ComponentId,
        name: Option<String>,
    },
    Expression(String),
}

#[derive(Clone, facet::Facet)]
#[facet(opaque)]
pub struct Binding {
    spec: BindingSpec,
    id: ComponentId,
    expression: Option<Expression>,
    error: Option<String>,
    attempted_generation: Option<u64>,
}

impl Default for Binding {
    fn default() -> Self {
        Self::from(ComponentId(0))
    }
}

impl From<ComponentId> for Binding {
    fn from(id: ComponentId) -> Self {
        Self {
            spec: if id == ComponentId(0) {
                BindingSpec::Unbound
            } else {
                BindingSpec::Component { id, name: None }
            },
            id,
            expression: None,
            error: None,
            attempted_generation: None,
        }
    }
}

impl Binding {
    pub fn from_expression(expression: Expression, text: String) -> Self {
        Self {
            id: expression.component_id(),
            spec: BindingSpec::Expression(format!("={}", expressions::body(&text))),
            expression: Some(expression),
            error: None,
            attempted_generation: None,
        }
    }

    pub fn id(&self) -> ComponentId {
        self.id
    }
    pub fn spec(&self) -> &BindingSpec {
        &self.spec
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn is_unbound(&self) -> bool {
        matches!(self.spec, BindingSpec::Unbound)
    }
    pub fn expression_text(&self) -> Option<String> {
        match &self.spec {
            BindingSpec::Expression(text) => Some(text.clone()),
            _ => None,
        }
    }

    /// Legacy string boundary. Preserve even an unavailable/invalid expression.
    pub fn from_text(text: &str, db: &Arc<DB>, cx: &mut App) -> Self {
        let mut binding = if text.is_empty() {
            Self::default()
        } else if expressions::is_expression(text) {
            Self {
                spec: BindingSpec::Expression(text.to_string()),
                ..Self::default()
            }
        } else {
            Self {
                spec: BindingSpec::Component {
                    id: ComponentId::new(text),
                    name: Some(text.to_string()),
                },
                ..Self::from(ComponentId::new(text))
            }
        };
        binding.resolve(db, cx);
        binding
    }

    /// Acquire a pick while the expression editor still owns its temporary.
    pub fn selected(id: ComponentId, text: &str, cx: &App) -> Self {
        if expressions::is_expression(text) {
            Self {
                spec: BindingSpec::Expression(text.to_string()),
                id,
                expression: expressions::running(id, cx),
                error: None,
                attempted_generation: None,
            }
        } else {
            Self {
                spec: BindingSpec::Component {
                    id,
                    name: Some(text.to_string()),
                },
                ..Self::from(id)
            }
        }
    }

    pub fn unresolved(id: ComponentId, expression: Option<String>) -> Self {
        match expression {
            Some(text) => Self {
                spec: BindingSpec::Expression(text),
                ..Self::default()
            },
            None => Self::from(id),
        }
    }

    /// Legacy id plus optional expression boundary, also used by trace pickers.
    pub fn from_legacy(id: ComponentId, text: Option<&str>, db: &Arc<DB>, cx: &mut App) -> Self {
        if let Some(text) = text {
            if expressions::is_expression(text) {
                return Self::from_text(text, db, cx);
            }
            let mut binding = Self::selected(id, text, cx);
            binding.resolve(db, cx);
            return binding;
        }
        let mut binding = Self::from(id);
        binding.resolve(db, cx);
        binding
    }

    /// Reconnect after producer registration. All consumers use this same rule.
    pub fn resolve(&mut self, db: &Arc<DB>, cx: &mut App) {
        if self.expression.is_some() || self.is_unbound() {
            return;
        }
        let generation = db.vtable_gen.latest();
        if self.attempted_generation == Some(generation) {
            return;
        }
        self.attempted_generation = Some(generation);
        if let BindingSpec::Component { id, name } = &mut self.spec {
            if let Some(text) = expressions::binding_text(db, *id) {
                self.spec = BindingSpec::Expression(text);
            } else {
                if name.is_none() {
                    *name =
                        db.with_state(|s| s.get_component_metadata(*id).map(|m| m.name.clone()));
                }
                return;
            }
        }
        if let BindingSpec::Expression(text) = &self.spec {
            match expressions::bind(text, db, cx) {
                Ok(bound) => {
                    self.id = bound.id;
                    self.expression = bound.expression;
                    self.error = None;
                }
                Err(error) => {
                    self.id = ComponentId(0);
                    self.error = Some(error.to_string());
                }
            }
        }
    }

    /// The canonical editor/legacy string representation, never a display label.
    pub fn text(&self, db: &DB) -> String {
        match &self.spec {
            BindingSpec::Unbound => String::new(),
            BindingSpec::Expression(text) => text.clone(),
            BindingSpec::Component { id, name } => expressions::binding_text(db, *id)
                .or_else(|| name.clone())
                .or_else(|| {
                    db.with_state(|s| s.get_component_metadata(*id).map(|m| m.name.clone()))
                })
                .unwrap_or_default(),
        }
    }
}

/// Shared observation and invalidation for inputs owned by non-rendered children.
/// A plot/model parent does not need a special inspector callback to rebind.
#[derive(Default)]
pub(crate) struct InputChanges {
    entries: std::collections::HashMap<
        gpui::EntityId,
        (
            Vec<(ComponentId, usize)>,
            gpui::Subscription,
            Vec<gpui::Task<()>>,
        ),
    >,
}

impl InputChanges {
    pub fn changed<T: 'static, E: 'static>(
        &mut self,
        inputs: &[gpui::Entity<T>],
        db: &Arc<DB>,
        key: impl Fn(&T) -> Vec<(ComponentId, usize)>,
        cx: &mut gpui::Context<E>,
    ) -> Vec<gpui::EntityId> {
        self.changed_with(inputs, db, key, |_, cx| cx.notify(), cx)
    }

    /// Observe configuration edits separately from the history watchers' repaints.
    pub fn changed_with<T: 'static, E: 'static>(
        &mut self,
        inputs: &[gpui::Entity<T>],
        db: &Arc<DB>,
        key: impl Fn(&T) -> Vec<(ComponentId, usize)>,
        on_edit: fn(&mut E, &mut gpui::Context<E>),
        cx: &mut gpui::Context<E>,
    ) -> Vec<gpui::EntityId> {
        self.entries
            .retain(|id, _| inputs.iter().any(|input| input.entity_id() == *id));
        let mut changed = Vec::new();
        for input in inputs {
            let id = input.entity_id();
            let want = key(input.read(cx));
            if let Some((previous, _, watchers)) = self.entries.get_mut(&id) {
                if *previous != want {
                    *watchers = want
                        .iter()
                        .filter(|(id, _)| *id != ComponentId(0))
                        .map(|(id, _)| watch_history(db.clone(), *id, cx))
                        .collect();
                    *previous = want;
                    changed.push(id);
                }
            } else {
                let subscription = cx.observe(input, move |this, _, cx| on_edit(this, cx));
                let watchers = want
                    .iter()
                    .filter(|(id, _)| *id != ComponentId(0))
                    .map(|(id, _)| watch_history(db.clone(), *id, cx))
                    .collect();
                self.entries.insert(id, (want, subscription, watchers));
            }
        }
        changed
    }
}

/// History orchestration shared by plots, maps and sample tables. Rendering
/// and LoD selection stay with the consumer; replay ownership stays here.
pub struct BoundHistory {
    pub component: metor_db::Component,
    pub plan: Option<crate::dynamic::ops::replay::ReplayPlan>,
}

impl BoundHistory {
    pub fn for_binding(binding: &Binding, db: &DB, cx: &App) -> Option<Self> {
        Some(Self {
            component: db.with_state(|s| s.get_component(binding.id()).cloned())?,
            plan: expressions::replay_plan(binding.id(), db, cx),
        })
    }

    pub fn extent(&self) -> Option<std::ops::Range<metor_proto::types::Timestamp>> {
        let mut ranges = std::iter::once(&self.component)
            .chain(self.plan.iter().flat_map(|p| p.ports.iter()))
            .filter_map(component_extent);
        let mut extent = ranges.next()?;
        for range in ranges {
            extent.start = extent.start.min(range.start);
            extent.end = extent.end.max(range.end);
        }
        Some(extent)
    }

    pub fn request(&self, range: std::ops::Range<metor_proto::types::Timestamp>, cx: &App) {
        hydrate(&self.component, range.clone(), cx);
        self.request_replay(range, cx);
    }

    /// Returns missing output ranges, also used to paint plot gap bands.
    pub fn request_replay(
        &self,
        range: std::ops::Range<metor_proto::types::Timestamp>,
        cx: &App,
    ) -> metor_db::manifest::RangeVec {
        let mut uncovered = metor_db::manifest::RangeVec::new();
        let Some(plan) = &self.plan else {
            return uncovered;
        };
        for port in &plan.ports {
            hydrate(port, range.clone(), cx);
            // Held inputs seed from their last value before the requested range.
            if let Some(span) = port
                .time_series
                .manifest()
                .spans
                .iter()
                .rev()
                .find(|span| span.seal.start_ts < range.start)
            {
                hydrate(port, span.seal.start_ts..range.start, cx);
            }
        }
        crate::backfill::wanted(&self.component, plan, range, &mut uncovered);
        if let Some(backfiller) = crate::backfill::backfiller(cx) {
            for range in &uncovered {
                backfiller.request(self.component.component_id, range.clone(), plan.clone());
            }
        }
        uncovered
    }
}

fn hydrate(
    component: &metor_db::Component,
    range: std::ops::Range<metor_proto::types::Timestamp>,
    cx: &App,
) {
    if let Some(hydrator) = crate::hydration::hydrator(cx) {
        let mut gaps = metor_db::manifest::GapVec::new();
        component.time_series.coverage(range, &mut gaps);
        for gap in gaps {
            if gap.state == metor_db::manifest::SpanState::RemoteOnly {
                hydrator.request(component.component_id, gap.range);
            }
        }
    }
}

pub fn component_extent(
    component: &metor_db::Component,
) -> Option<std::ops::Range<metor_proto::types::Timestamp>> {
    use metor_proto::types::Timestamp;
    let series = &component.time_series;
    let manifest = series.manifest();
    let start = series
        .start_timestamp()
        .into_iter()
        .chain(manifest.spans.first().map(|s| s.seal.start_ts))
        .min()?;
    let end = series
        .latest()
        .map(|s| Timestamp(s.timestamp().0.saturating_add(1)))
        .into_iter()
        .chain(
            manifest
                .spans
                .last()
                .map(|s| Timestamp(s.cover_end.0.saturating_add(1))),
        )
        .max()?;
    (start < end).then_some(start..end)
}

/// Repaint when old history is installed, including when the live head stays
/// unchanged. Input hydration also wakes the view to retry its replay request.
pub(crate) fn watch_history<E: 'static>(
    db: Arc<DB>,
    id: ComponentId,
    cx: &mut gpui::Context<E>,
) -> gpui::Task<()> {
    cx.spawn(async move |this, cx| {
        let component = crate::wait_for_component(&db, id).await;
        let plan = this
            .update(cx, |_, cx| expressions::replay_plan(id, &db, cx))
            .ok()
            .flatten();
        let components: Vec<_> = std::iter::once(component)
            .chain(plan.into_iter().flat_map(|p| p.ports))
            .collect();
        loop {
            let mut waits: Vec<_> = components
                .iter()
                .map(|c| Box::pin(c.time_series.wait()))
                .collect();
            std::future::poll_fn(|cx| {
                use std::future::Future;
                if waits
                    .iter_mut()
                    .any(|wait| wait.as_mut().poll(cx).is_ready())
                {
                    std::task::Poll::Ready(())
                } else {
                    std::task::Poll::Pending
                }
            })
            .await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        }
    })
}

/// Inputs restored before their producer registers must get another resolve
/// attempt even when no value stream has started yet.
pub(crate) fn watch_registrations<E: 'static>(
    db: Arc<DB>,
    cx: &mut gpui::Context<E>,
) -> gpui::Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            db.vtable_gen.wait().await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        }
    })
}
