//! Where an image view's frames come from.

use std::cell::RefCell;
use std::sync::Arc;

use gpui::{App, Context, Entity, SharedString, Subscription};
use metor_db::DB;
use metor_proto::types::{ComponentId, Timestamp};

use super::layout::{FrameLayout, LayoutError};
use super::view::ImageView;
use crate::temporal::samples::{SampleStatus, SelectedReader, acquire};

/// One sample of an image component.
#[derive(Clone, Debug)]
pub struct Frame {
    pub timestamp: Timestamp,
    pub bytes: Arc<[u8]>,
    pub layout: FrameLayout,
}

#[derive(Clone, Debug)]
pub enum FrameState {
    Ready {
        frame: Frame,
        stale: bool,
    },
    Loading,
    /// No sample at or before the view time.
    Missing,
    Error(SharedString),
}

/// A view's frames. `observe` subscribes the view to changes.
pub trait FrameSource {
    fn frame(&self, cx: &App) -> FrameState;
    fn observe(&self, cx: &mut Context<ImageView>) -> Option<Subscription>;
}

/// The selected sample of an image component, following the view time.
pub struct RecordSource {
    db: Arc<DB>,
    id: ComponentId,
    reader: Entity<SelectedReader>,
    /// The layout and the `(vtable_gen, metadata_gen)` it was read at.
    layout: RefCell<Option<((u64, u64), Option<Result<FrameLayout, LayoutError>>)>>,
}

impl RecordSource {
    pub(crate) fn new(db: Arc<DB>, id: ComponentId, cx: &mut App) -> Self {
        let reader = acquire(db.clone(), id, cx);
        Self {
            db,
            id,
            reader,
            layout: RefCell::new(None),
        }
    }

    /// `None` while the component is not registered.
    fn layout(&self) -> Option<Result<FrameLayout, LayoutError>> {
        let generation = (self.db.vtable_gen.latest(), self.db.metadata_gen.latest());
        let mut cache = self.layout.borrow_mut();
        if let Some((seen, layout)) = cache.as_ref()
            && *seen == generation
        {
            return *layout;
        }
        let layout = self.db.with_state(|state| {
            let meta = state.get_component_metadata(self.id)?;
            let schema = &state.get_component(self.id)?.schema;
            Some(FrameLayout::from_component(schema, meta))
        });
        *cache = Some((generation, layout));
        layout
    }
}

impl FrameSource for RecordSource {
    fn frame(&self, cx: &App) -> FrameState {
        let layout = match self.layout() {
            None => return FrameState::Loading,
            Some(Err(err)) => return FrameState::Error(err.to_string().into()),
            Some(Ok(layout)) => layout,
        };
        let selection = &self.reader.read(cx).selection;
        match (&selection.status, &selection.sample) {
            (SampleStatus::Error(err), _) => FrameState::Error(err.clone().into()),
            (SampleStatus::Missing, _) => FrameState::Missing,
            (_, Some(sample)) => ready(layout, sample.timestamp, &sample.bytes, selection.stale),
            (_, None) => FrameState::Loading,
        }
    }

    fn observe(&self, cx: &mut Context<ImageView>) -> Option<Subscription> {
        Some(cx.observe(&self.reader, |view, _, cx| view.source_changed(cx)))
    }
}

/// A sample whose bytes match its layout, or the mismatch as an error.
fn ready(layout: FrameLayout, timestamp: Timestamp, bytes: &Arc<[u8]>, stale: bool) -> FrameState {
    match layout.pixels(bytes) {
        Ok(_) => FrameState::Ready {
            frame: Frame {
                timestamp,
                bytes: bytes.clone(),
                layout,
            },
            stale,
        },
        Err(err) => FrameState::Error(err.to_string().into()),
    }
}

/// One fixed frame, for views without a db.
pub struct StaticSource(pub FrameState);

impl FrameSource for StaticSource {
    fn frame(&self, _cx: &App) -> FrameState {
        self.0.clone()
    }

    fn observe(&self, _cx: &mut Context<ImageView>) -> Option<Subscription> {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use metor_db::ComponentSchema;
    use metor_db::manifest::SpanSource;
    use metor_proto::types::PrimType;
    use metor_proto_wkt::{ComponentMetadata, ImageFormat};

    use super::*;
    use crate::temporal::{TemporalController, TimeAction, TimeExpr, dispatch};

    pub(crate) const ID: &str = "cam.pixels";

    /// A `U16 [4, 4]` `mono16/12` component; every pixel of frame `t` is
    /// `t * 10`.
    pub(crate) fn scrub_db(stamps: &[i64]) -> (tempfile::TempDir, Arc<DB>, metor_db::Component) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        let meta = ComponentMetadata::from(ID).with_image(ImageFormat::Mono16, 12);
        let id = meta.component_id;
        db.with_state_mut(|s| {
            s.set_component_metadata(meta, &db.path)?;
            s.insert_component(
                id,
                ComponentSchema::new(PrimType::U16, &[4usize, 4][..]),
                &db.path,
            )
        })
        .unwrap();
        let component = db.with_state(|s| s.get_component(id).cloned()).unwrap();
        install(&component, stamps);
        (temp, db, component)
    }

    pub(crate) fn frame_bytes(t: i64) -> Vec<u8> {
        (0..16)
            .flat_map(|_| ((t * 10) as u16).to_le_bytes())
            .collect()
    }

    pub(crate) fn install(c: &metor_db::Component, stamps: &[i64]) {
        let bytes: Vec<Vec<u8>> = stamps.iter().map(|&t| frame_bytes(t)).collect();
        c.time_series
            .install_samples(
                32,
                stamps
                    .iter()
                    .zip(&bytes)
                    .map(|(s, b)| (Timestamp(*s), b.as_slice())),
                SpanSource::RemoteFetch,
            )
            .unwrap();
    }

    pub(crate) fn seek(t: i64, cx: &mut gpui::TestAppContext) {
        cx.update(|cx| dispatch(TimeAction::Seek(TimeExpr::fixed(Timestamp(t))), cx).unwrap());
        cx.run_until_parked();
    }

    /// The timestamp and pixel (0, 0) of the source's frame.
    fn read(source: &RecordSource, cx: &mut gpui::TestAppContext) -> Option<(i64, u16)> {
        cx.update(|cx| match source.frame(cx) {
            FrameState::Ready { frame, .. } => Some((
                frame.timestamp.0,
                u16::from_le_bytes([frame.bytes[0], frame.bytes[1]]),
            )),
            FrameState::Missing => None,
            other => panic!("unexpected {other:?}"),
        })
    }

    #[gpui::test]
    fn scrubbing_selects_the_right_sample(cx: &mut gpui::TestAppContext) {
        let (_temp, db, component) = scrub_db(&[10, 20, 100]);
        let source = cx.update(|cx| {
            TemporalController::init(db.clone(), cx);
            RecordSource::new(db.clone(), component.component_id, cx)
        });
        for (t, want) in [
            (15, Some(10)),
            (20, Some(20)),
            (99, Some(20)),
            (5, None),
            (100, Some(100)),
        ] {
            seek(t, cx);
            assert_eq!(
                read(&source, cx),
                want.map(|w| (w, (w * 10) as u16)),
                "seek {t}"
            );
        }
        // Paused at 99, a late arrival before the view time wins.
        seek(99, cx);
        component.push_buf(Timestamp(50), &frame_bytes(50)).unwrap();
        cx.run_until_parked();
        assert_eq!(read(&source, cx), Some((50, 500)));
        // Live, a committed append (what the db's WAL committer makes) leads.
        cx.update(|cx| dispatch(TimeAction::Live, cx).unwrap());
        let mut writer = component.time_series.writer().unwrap();
        writer.push_buf(Timestamp(120), &frame_bytes(120)).unwrap();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(100));
        cx.run_until_parked();
        assert_eq!(read(&source, cx), Some((120, 1200)));
    }

    #[gpui::test]
    fn a_bad_layout_is_an_error(cx: &mut gpui::TestAppContext) {
        let (_temp, db, component) = scrub_db(&[10]);
        let mut meta = ComponentMetadata::from(ID);
        metor_proto_wkt::MetadataExt::set(&mut meta, metor_proto_wkt::IMAGE_FORMAT_KEY, "mono12");
        db.with_state_mut(|s| s.set_component_metadata(meta, &db.path))
            .unwrap();
        let source = cx.update(|cx| {
            TemporalController::init(db.clone(), cx);
            RecordSource::new(db.clone(), component.component_id, cx)
        });
        seek(10, cx);
        let state = cx.update(|cx| source.frame(cx));
        assert!(
            matches!(&state, FrameState::Error(e) if e.as_ref() == "unknown image.format"),
            "{state:?}"
        );
    }

    #[gpui::test]
    fn an_unregistered_component_is_loading(cx: &mut gpui::TestAppContext) {
        let (_temp, db, _component) = scrub_db(&[10]);
        let source = cx.update(|cx| {
            TemporalController::init(db.clone(), cx);
            RecordSource::new(db.clone(), ComponentId::new("cam.gone"), cx)
        });
        seek(10, cx);
        assert!(matches!(
            cx.update(|cx| source.frame(cx)),
            FrameState::Loading
        ));
    }
}
