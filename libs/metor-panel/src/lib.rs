// gpui is single-threaded by design, so `Arc<dyn Fn>` is the load-bearing
// shape for every event handler / build closure in this crate. The
// `arc_with_non_send_sync` lint flags this everywhere, but switching to
// `Rc` isn't possible — gpui's own APIs require `Arc`. Same story for
// `type_complexity`: the closures' fully-spelled-out types fall out of
// gpui's API and adding type aliases everywhere would mostly just shuffle
// the noise around.
#![allow(clippy::arc_with_non_send_sync, clippy::type_complexity)]

extern crate self as metor_panel;

use std::mem::size_of;

use metor_db::disruptor::{ReadGrant, Reader};
use metor_db::{Component, ComponentSchema, DB};
use metor_proto::types::{ComponentId, Timestamp};
pub mod alarms;
pub mod app;
pub(crate) mod atlas;
pub mod backfill;
pub(crate) mod background_tasks;
pub mod config;
pub mod connections;
pub mod data_binding;
pub mod dynamic;
pub mod gpu_context;
pub(crate) mod graph_canvas;
pub mod hydration;
pub mod icons;
pub mod inspect;
pub mod inspector;
pub mod logs;
pub(crate) mod motion;
pub(crate) mod msg_ingest;
pub mod plot_events;
pub mod presets;
pub mod query;
pub mod sequences;
pub(crate) mod session;
pub mod temporal;
pub mod theme;
pub mod tiles;
pub mod transient;
pub mod views;
pub(crate) mod window_controls;
pub mod wiring;
pub(crate) mod workspace;

pub use app::PanelApp;
pub use connections::{
    AddressResolver, ConnectContext, Connected, ConnectionBackend, ConnectionOption,
    ConnectionOptions, ConnectionStatus, ConnectionTarget, OptionKind, OptionSpec, OptionValue,
    RegistryHandle, Resolved, TargetId,
};
pub use inspector::palette::{Category, InspectionItem, ItemProvider};

/// Streams the most recent value of a component from its WAL.
///
/// Each `next()` yields a [`WalView`] pointing at the last complete message
/// in the grant. Earlier messages in the same grant are skipped: readers only
/// need the latest sample to repaint.
pub(crate) struct WalComponentStream {
    reader: Reader,
    schema: ComponentSchema,
}

impl WalComponentStream {
    pub(crate) fn new(component: &Component) -> Self {
        Self {
            reader: component.wal.reader(),
            schema: component.schema.clone(),
        }
    }

    pub(crate) async fn next(&mut self) -> WalView<'_> {
        let msg_size = self.schema.size() + size_of::<Timestamp>();
        let grant = self.reader.next().await;
        let count = (grant.len() / msg_size).max(1);
        let offset = (count - 1) * msg_size + size_of::<Timestamp>();
        WalView {
            grant,
            size: self.schema.size(),
            offset,
        }
    }
}

/// Borrowed view into the last value of a WAL grant.
pub(crate) struct WalView<'a> {
    grant: ReadGrant<'a>,
    size: usize,
    offset: usize,
}

impl WalView<'_> {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.grant[self.offset..self.offset + self.size]
    }

    /// The `[Timestamp]` framed just ahead of the value. Staleness is judged
    /// by this, not by arrival, so a replay and a slow link both read true.
    pub(crate) fn sample_time(&self) -> Option<Timestamp> {
        let start = self.offset - size_of::<Timestamp>();
        let bytes: [u8; 8] = self.grant[start..self.offset].try_into().ok()?;
        Some(Timestamp::from_le_bytes(bytes))
    }
}

pub(crate) async fn wait_for_component(db: &DB, component_id: ComponentId) -> Component {
    loop {
        let seen = db.vtable_gen.latest();
        if let Some(component) = db.with_state(|state| state.get_component(component_id).cloned()) {
            return component;
        }
        vtable_changed(db, seen).await;
    }
}

/// Resolve with the output of whichever of `futures` is ready first, polling
/// them in order so earlier entries win a simultaneous wake.
pub(crate) async fn first_ready<F: std::future::Future + Unpin>(futures: &mut [F]) -> F::Output {
    std::future::poll_fn(|cx| {
        for future in futures.iter_mut() {
            if let std::task::Poll::Ready(output) = std::pin::Pin::new(future).poll(cx) {
                return std::task::Poll::Ready(output);
            }
        }
        std::task::Poll::Pending
    })
    .await
}

/// Wait for the vtable to move past generation `seen`.
///
/// Read `seen` before looking at the components it guards: a registration
/// that lands while the caller looks then still wakes it, where a bare
/// `vtable_gen.wait()` would sleep through it.
pub(crate) async fn vtable_changed(db: &DB, seen: u64) {
    db.vtable_gen
        .wait_for(|generation| generation != seen)
        .await;
}
