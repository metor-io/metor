// gpui is single-threaded by design, so `Arc<dyn Fn>` is the load-bearing
// shape for every event handler / build closure in this crate. The
// `arc_with_non_send_sync` lint flags this everywhere, but switching to
// `Rc` isn't possible — gpui's own APIs require `Arc`. Same story for
// `type_complexity`: the closures' fully-spelled-out types fall out of
// gpui's API and adding type aliases everywhere would mostly just shuffle
// the noise around.
#![allow(clippy::arc_with_non_send_sync, clippy::type_complexity)]

use std::mem::size_of;

use metor_db::disruptor::{ReadGrant, Reader};
use metor_db::{Component, ComponentSchema, DB};
use metor_proto::types::{ComponentId, ComponentView, Timestamp};
pub mod alarms;
pub mod logs;
pub mod app;
pub mod config;
pub mod connections;
pub mod dynamic;
pub mod gpu_context;
pub(crate) mod graph_canvas;
pub(crate) mod graph_layout;
pub mod hydration;
pub mod icons;
pub mod inspect;
pub mod inspector;
pub(crate) mod msg_ingest;
pub mod node_editor;
pub mod plot_events;
pub mod sequences;
pub mod presets;
pub mod theme;
pub mod tiles;
pub mod transient;
pub mod views;
pub mod wiring;
pub(crate) mod window_controls;

pub use app::PanelApp;
pub use connections::{
    AddressResolver, ConnectContext, Connected, ConnectionBackend, ConnectionStatus,
    ConnectionTarget, RegistryHandle, TargetId,
};
pub use inspector::palette::{Category, InspectionItem, ItemProvider};

/// Borrow as a [`ComponentView`] without copying the backing buffer.
pub trait AsComponentView {
    fn as_component_view(&self) -> ComponentView<'_>;
}

impl AsComponentView for ComponentView<'_> {
    fn as_component_view(&self) -> ComponentView<'_> {
        *self
    }
}

/// Async source of component value updates.
///
/// Views borrow from the stream; hold the stream across `.await` points.
pub trait ComponentStream {
    type View<'a>: AsComponentView
    where
        Self: 'a;
    fn next(&mut self) -> impl std::future::Future<Output = Self::View<'_>>;
}

/// Resolves a component (by handle, id, or dynamic node) into a [`ComponentStream`].
///
/// The `ComponentId` impl waits for the component to appear in the DB,
/// letting views subscribe before the producer has registered.
pub trait ComponentStreamBuilder {
    type Stream: ComponentStream + Send;
    fn component_id(&self) -> ComponentId;
    fn into_stream(self, db: &DB) -> impl std::future::Future<Output = Self::Stream> + Send;
}

impl ComponentStreamBuilder for Component {
    type Stream = WalComponentStream;

    fn component_id(&self) -> ComponentId {
        self.component_id
    }

    async fn into_stream(self, _db: &DB) -> WalComponentStream {
        WalComponentStream::new(&self)
    }
}

impl ComponentStreamBuilder for ComponentId {
    type Stream = WalComponentStream;

    fn component_id(&self) -> ComponentId {
        *self
    }

    async fn into_stream(self, db: &DB) -> WalComponentStream {
        let component = wait_for_component(db, self).await;
        WalComponentStream::new(&component)
    }
}

/// Streams the most recent value of a component from its WAL.
///
/// Each `next()` yields a [`WalView`] pointing at the last complete message
/// in the grant. Earlier messages in the same grant are skipped: views only
/// need the latest sample to repaint.
pub struct WalComponentStream {
    reader: Reader,
    schema: ComponentSchema,
}

impl WalComponentStream {
    pub fn new(component: &Component) -> Self {
        Self::from_disruptor(&component.wal, component.schema.clone())
    }

    /// Subscribe to any [`Disruptor`] whose messages are framed as
    /// `[Timestamp][value bytes]`. Used by `dynamic` nodes to expose
    /// themselves to existing views.
    pub fn from_disruptor(
        disruptor: &metor_db::disruptor::Disruptor,
        schema: ComponentSchema,
    ) -> Self {
        Self {
            reader: disruptor.reader(),
            schema,
        }
    }
}

/// Borrowed view into the last value of a WAL grant.
///
/// The grant is retained so `parse_value` can lazily materialize the
/// [`ComponentView`] against a buffer that stays alive for `'a`.
pub struct WalView<'a> {
    _grant: ReadGrant<'a>,
    schema: &'a ComponentSchema,
    offset: usize,
}

impl AsComponentView for WalView<'_> {
    fn as_component_view(&self) -> ComponentView<'_> {
        let value_buf = &self._grant[self.offset..];
        let (_size, view) = self
            .schema
            .parse_value(value_buf)
            .expect("invalid WAL data");
        view
    }
}

impl ComponentStream for WalComponentStream {
    type View<'a> = WalView<'a>;

    async fn next(&mut self) -> WalView<'_> {
        let msg_size = self.schema.size() + size_of::<Timestamp>();
        let grant = self.reader.next().await;
        let count = (grant.len() / msg_size).max(1);
        let offset = (count - 1) * msg_size + size_of::<Timestamp>();
        WalView {
            _grant: grant,
            schema: &self.schema,
            offset,
        }
    }
}

pub(crate) async fn wait_for_component(db: &DB, component_id: ComponentId) -> Component {
    loop {
        if let Some(component) = db.with_state(|state| state.get_component(component_id).cloned()) {
            return component;
        }
        db.vtable_gen.wait().await;
    }
}
