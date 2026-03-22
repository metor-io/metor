use std::mem::size_of;

use metor_db::disruptor::{ReadGrant, Reader};
use metor_db::{Component, ComponentSchema, DB};
use metor_proto::types::{ComponentId, ComponentView, Timestamp};

pub mod elements;
pub mod theme;

pub trait AsComponentView {
    fn as_component_view(&self) -> ComponentView<'_>;
}

impl AsComponentView for ComponentView<'_> {
    fn as_component_view(&self) -> ComponentView<'_> {
        *self
    }
}

pub trait ComponentStream {
    type View<'a>: AsComponentView
    where
        Self: 'a;
    fn next(&mut self) -> impl std::future::Future<Output = Self::View<'_>>;
}

pub trait ComponentStreamBuilder {
    fn into_stream(self, db: &DB) -> impl std::future::Future<Output = WalComponentStream> + Send;
}

impl ComponentStreamBuilder for Component {
    async fn into_stream(self, _db: &DB) -> WalComponentStream {
        WalComponentStream::new(&self)
    }
}

impl ComponentStreamBuilder for ComponentId {
    async fn into_stream(self, db: &DB) -> WalComponentStream {
        let component = wait_for_component(db, self).await;
        WalComponentStream::new(&component)
    }
}

pub struct WalComponentStream {
    reader: Reader,
    schema: ComponentSchema,
}

impl WalComponentStream {
    pub fn new(component: &Component) -> Self {
        Self {
            reader: component.wal.reader(),
            schema: component.schema.clone(),
        }
    }
}

/// A parsed view into the last value from a WAL read grant.
/// Holds the grant to keep the underlying buffer alive.
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
        // Point to the value portion of the last complete message
        let offset = (count - 1) * msg_size + size_of::<Timestamp>();
        WalView {
            _grant: grant,
            schema: &self.schema,
            offset,
        }
    }
}

async fn wait_for_component(db: &DB, component_id: ComponentId) -> Component {
    loop {
        if let Some(component) = db.with_state(|state| state.get_component(component_id).cloned()) {
            return component;
        }
        db.vtable_gen.wait().await;
    }
}
