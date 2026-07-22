//! Ingests the control system's [`WiringManifest`] broadcast into a single
//! app-global store the System Graph tile observes.
//!
//! The manifest carries the whole target wiring IR as JSON (`metor-fsw-2`'s
//! [`Wiring`]), re-broadcast on startup and on reload — the same telemetry
//! pub/sub pattern [`SequenceRegistry`](metor_proto_wkt::SequenceRegistry)
//! uses. There is exactly one live topology, so the fold is latest-wins: each
//! manifest wholly replaces the last. Decode failures never panic — a bad
//! payload or a version the panel doesn't understand is held as an error
//! string in state, leaving the previous good topology in place so the tile
//! keeps rendering while surfacing the problem.
//!
//! State folding lives in [`WiringState`] (pure, unit-tested). [`WiringStore`]
//! is the gpui entity that owns it plus the reader task; [`GlobalWiringStore`]
//! hands the entity to any view via [`try_global`].

use std::sync::Arc;

use gpui::{App, Context, Entity, Global, SharedString, Task, prelude::*};
use metor_db::DB;
use metor_fsw_2::ir::{IR_VERSION, Wiring};
use metor_proto::types::{Msg, Timestamp};
use metor_proto_wkt::WiringManifest;

use crate::msg_ingest::{IngestSource, ingest_all};

#[cfg(test)]
mod tests;

/// The folded wiring state: the latest decoded topology, or the error from the
/// most recent manifest that failed to decode. Kept free of gpui/DB so the
/// fold rules can be tested directly.
#[derive(Default)]
pub struct WiringState {
    wiring: Option<Wiring>,
    error: Option<SharedString>,
    updated_at: Option<Timestamp>,
}

impl WiringState {
    /// Apply one manifest, replacing any previous topology. The version gate
    /// runs before parsing (the manifest's `ir_version` rides alongside the
    /// JSON precisely so a consumer can reject a foreign-generation payload
    /// without parsing it); a mismatch or a parse failure is held as an error
    /// and leaves the previous good topology untouched.
    pub fn apply(&mut self, timestamp: Timestamp, manifest: WiringManifest) {
        self.updated_at = Some(timestamp);
        if manifest.ir_version != IR_VERSION {
            self.error = Some(
                format!(
                    "wiring IR version {} unsupported (panel understands {IR_VERSION})",
                    manifest.ir_version
                )
                .into(),
            );
            return;
        }
        match serde_json::from_str::<Wiring>(&manifest.ir_json) {
            Ok(wiring) => {
                self.wiring = Some(wiring);
                self.error = None;
            }
            Err(err) => {
                self.error = Some(format!("wiring manifest decode failed: {err}").into());
            }
        }
    }

    /// The latest decoded topology, or `None` before any good manifest arrives.
    pub fn wiring(&self) -> Option<&Wiring> {
        self.wiring.as_ref()
    }

    /// The error from the most recent manifest that failed to decode, if the
    /// last manifest was bad. Cleared by the next good manifest.
    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    /// Timestamp of the most recently applied manifest (good or bad).
    pub fn updated_at(&self) -> Option<Timestamp> {
        self.updated_at
    }
}

/// The gpui entity wrapping [`WiringState`]; owns the in-process ingestion task.
pub struct WiringStore {
    state: WiringState,
    _task: Task<()>,
}

/// Hands the shared [`WiringStore`] entity to any part of the app.
pub struct GlobalWiringStore(pub Entity<WiringStore>);

impl Global for GlobalWiringStore {}

/// The shared wiring store, or `None` if it was never initialized (e.g. in tests).
pub fn try_global(cx: &App) -> Option<Entity<WiringStore>> {
    cx.try_global::<GlobalWiringStore>().map(|g| g.0.clone())
}

impl WiringStore {
    pub fn init(db: Arc<DB>, cx: &mut App) {
        let entity = cx.new(|cx| WiringStore::new(db, cx));
        cx.set_global(GlobalWiringStore(entity));
    }

    fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let task = cx.spawn(async move |this, cx| {
            let sources = vec![IngestSource::new(
                WiringManifest::ID,
                |store: &mut Self, ts, manifest| store.state.apply(ts, manifest),
            )];
            ingest_all(db, sources, this, cx).await
        });

        Self {
            state: WiringState::default(),
            _task: task,
        }
    }

    pub fn state(&self) -> &WiringState {
        &self.state
    }
}
