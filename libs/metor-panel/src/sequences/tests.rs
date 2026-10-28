use std::sync::Arc;

use gpui::TestAppContext;
use metor_db::DB;
use metor_proto::types::{ComponentId, Msg, PrimType, Timestamp};
use metor_proto_wkt::{
    SequenceAccepts, SequenceChannelComponents, SequenceChannelSpec, SequenceChannelState,
    SequenceRegistry,
};

use super::{ChannelStatusReader, SequenceHistory, SequenceState, format_age};
use crate::msg_ingest::{IngestSource, apply_backfill};
use crate::temporal::{TimeAction, TimeExpr};

fn spec(name: &str, available: &[&str]) -> SequenceChannelSpec {
    SequenceChannelSpec {
        name: name.to_string(),
        available: available.iter().map(|s| (*s).to_string()).collect(),
        ..Default::default()
    }
}

fn registry(channels: &[(&str, &[&str])]) -> SequenceRegistry {
    SequenceRegistry {
        channels: channels
            .iter()
            .map(|(name, available)| spec(name, available))
            .collect(),
    }
}

#[test]
fn registry_declares_channels_in_order() {
    let mut state = SequenceState::default();
    state.apply_registry(registry(&[("deploy", &["a", "b"]), ("attitude", &["c"])]));

    let channels = state.channels_ordered();
    assert_eq!(channels.len(), 2);
    assert_eq!(channels[0].name.as_ref(), "deploy");
    assert_eq!(channels[1].name.as_ref(), "attitude");
    assert_eq!(channels[0].available.len(), 2);
}

#[test]
fn registry_names_each_status_component_in_full() {
    let mut state = SequenceState::default();
    state.apply_registry(SequenceRegistry {
        channels: vec![SequenceChannelSpec {
            system: "dl_slot".into(),
            status: SequenceChannelComponents {
                state: "fsw.deploy.channel_status.state".into(),
                message_at: "fsw.deploy.channel_status.message_at".into(),
                ..Default::default()
            },
            ..spec("deploy", &[])
        }],
    });

    let deploy = state.channel("deploy").unwrap();
    assert_eq!(deploy.system.as_ref(), "dl_slot");
    assert_eq!(
        deploy.components.state,
        Some(ComponentId::new("fsw.deploy.channel_status.state"))
    );
    assert_eq!(
        deploy.components.message_at,
        Some(ComponentId::new("fsw.deploy.channel_status.message_at"))
    );
    // An empty name is a field the channel doesn't publish, not a component
    // named "".
    assert_eq!(deploy.components.elapsed, None);
}

#[test]
fn the_loaded_index_names_a_sequence() {
    let mut state = SequenceState::default();
    state.apply_registry(registry(&[("deploy", &["solar", "lunar"])]));
    let deploy = state.channel("deploy").unwrap();
    assert_eq!(deploy.sequence(Some(1)).as_deref(), Some("lunar"));
    assert_eq!(deploy.sequence(Some(2)), None);
    assert_eq!(deploy.sequence(None), None);
}

#[test]
fn a_new_registry_replaces_the_declaration() {
    let mut state = SequenceState::default();
    state.apply_registry(registry(&[("a", &[]), ("b", &[]), ("c", &[])]));
    state.apply_registry(registry(&[("c", &["x"]), ("a", &[])]));

    let names: Vec<&str> = state
        .channels_ordered()
        .iter()
        .map(|c| c.name.as_ref())
        .collect();
    assert_eq!(names, ["c", "a"]);
    assert!(state.channel("b").is_none());
    assert_eq!(state.channel("c").unwrap().available.len(), 1);
}

/// The store's one ingest source, folding into a bare [`SequenceState`], so the
/// backfill path runs without a gpui `App`.
#[test]
fn backfill_keeps_the_latest_registry() {
    let mut state = SequenceState::default();
    let mut sources = vec![IngestSource::new(
        SequenceRegistry::ID,
        |s: &mut SequenceState, _ts, reg| s.apply_registry(reg),
    )];
    let bytes = |r: &SequenceRegistry| postcard::to_allocvec(r).unwrap();
    let entries = vec![
        (Timestamp(2), 0, bytes(&registry(&[("new", &[])]))),
        (Timestamp(1), 0, bytes(&registry(&[("old", &[])]))),
    ];
    apply_backfill(&mut state, &mut sources, entries);
    assert!(state.channel("new").is_some());
    assert!(state.channel("old").is_none());
}

#[test]
fn ages_read_in_the_largest_whole_unit() {
    let at = Timestamp(0);
    assert_eq!(format_age(at, Timestamp(2_000_000)), "2s ago");
    assert_eq!(format_age(at, Timestamp(300_000_000)), "5m ago");
    assert_eq!(format_age(at, Timestamp(5_400_000_000)), "1.5h ago");
    assert_eq!(format_age(Timestamp(10), at), "0s ago");
}

/// One cycle of a channel's telemetered state, as fsw-3's frame carries it.
#[derive(Clone)]
pub(crate) struct Frame {
    pub state: SequenceChannelState,
    pub loaded: u32,
    pub accepts: u32,
    pub artifact: &'static str,
    pub message: &'static str,
    pub message_at: i64,
    pub elapsed: f64,
    pub abort_elapsed: f64,
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            state: SequenceChannelState::Unloaded,
            loaded: u32::MAX,
            accepts: 0,
            artifact: "",
            message: "",
            message_at: 0,
            elapsed: 0.0,
            abort_elapsed: 0.0,
        }
    }
}

/// `message` is a `FrameStr<124>`: a NUL-terminated `U8` tensor of this length.
const MESSAGE: usize = 124;
/// `artifact` is a `FrameStr<64>`.
const ARTIFACT: usize = 64;

fn field(channel: &str, field: &str) -> String {
    format!("fsw.{channel}.channel_status.{field}")
}

/// The component names fsw-3 gives `channel`'s state.
pub(crate) fn components(channel: &str) -> SequenceChannelComponents {
    SequenceChannelComponents {
        state: field(channel, "state"),
        loaded: field(channel, "loaded"),
        artifact: field(channel, "artifact"),
        hash: field(channel, "hash"),
        accepts: field(channel, "accepts"),
        message: field(channel, "message"),
        message_at: field(channel, "message_at"),
        elapsed: field(channel, "elapsed"),
        abort_elapsed: field(channel, "abort_elapsed"),
    }
}

fn text(s: &str, len: usize) -> Vec<u8> {
    let mut bytes = vec![0; len];
    bytes[..s.len()].copy_from_slice(s.as_bytes());
    bytes
}

type Encode = fn(&Frame) -> Vec<u8>;

/// Record `frames` (each at its sample stamp) into `channel`'s components as
/// resident history, registering the components first if need be.
pub(crate) fn install_status(db: &DB, channel: &str, frames: &[(i64, Frame)]) {
    let fields: [(&str, PrimType, &[usize], Encode); 8] = [
        ("state", PrimType::U32, &[], |f| {
            f.state.code().to_le_bytes().to_vec()
        }),
        ("loaded", PrimType::U32, &[], |f| {
            f.loaded.to_le_bytes().to_vec()
        }),
        ("accepts", PrimType::U32, &[], |f| {
            f.accepts.to_le_bytes().to_vec()
        }),
        ("artifact", PrimType::U8, &[ARTIFACT], |f| {
            text(f.artifact, ARTIFACT)
        }),
        ("message", PrimType::U8, &[MESSAGE], |f| {
            text(f.message, MESSAGE)
        }),
        ("message_at", PrimType::I64, &[], |f| {
            f.message_at.to_le_bytes().to_vec()
        }),
        ("elapsed", PrimType::F64, &[], |f| {
            f.elapsed.to_le_bytes().to_vec()
        }),
        ("abort_elapsed", PrimType::F64, &[], |f| {
            f.abort_elapsed.to_le_bytes().to_vec()
        }),
    ];
    for (name, prim, shape, encode) in fields {
        let id = ComponentId::new(&field(channel, name));
        let schema = metor_db::ComponentSchema::new(prim, shape);
        let size = schema.size();
        if db.with_state(|s| s.get_component(id).is_none()) {
            db.with_state_mut(|s| s.insert_component(id, schema, &db.path))
                .unwrap();
        }
        let component = db.with_state(|s| s.get_component(id).cloned()).unwrap();
        let samples: Vec<(Timestamp, Vec<u8>)> = frames
            .iter()
            .map(|(at, frame)| (Timestamp(*at), encode(frame)))
            .collect();
        component
            .time_series
            .install_samples(
                size,
                samples.iter().map(|(t, b)| (*t, b.as_slice())),
                metor_db::manifest::SpanSource::RemoteFetch,
            )
            .unwrap();
    }
    // What a vtable registration does, so tasks waiting on the components
    // find them.
    db.vtable_gen
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// A registry declaring `channel` with `available`, its state in the
/// components [`components`] names.
pub(crate) fn declare(channel: &str, available: &[&str], system: &str) -> SequenceRegistry {
    SequenceRegistry {
        channels: vec![SequenceChannelSpec {
            system: system.into(),
            status: components(channel),
            ..spec(channel, available)
        }],
    }
}

/// A db with a sequence store whose registry is `registry`, and the time
/// controller every reader follows.
fn app(cx: &mut TestAppContext, registry: &SequenceRegistry) -> (Arc<DB>, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
    cx.update(|cx| {
        crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
        crate::temporal::TemporalController::init(db.clone(), cx);
        super::SequenceStore::init(db.clone(), cx);
    });
    cx.run_until_parked();
    db.push_msg(
        Timestamp(1),
        SequenceRegistry::ID,
        &postcard::to_allocvec(registry).unwrap(),
    )
    .unwrap();
    cx.run_until_parked();
    (db, temp)
}

fn acquire(cx: &mut TestAppContext, channel: &str) -> gpui::Entity<ChannelStatusReader> {
    let reader = cx
        .update(|cx| ChannelStatusReader::acquire(channel.to_string().into(), cx))
        .expect("store installed");
    cx.run_until_parked();
    reader
}

#[gpui::test]
fn the_reader_decodes_every_field(cx: &mut TestAppContext) {
    let (db, _temp) = app(cx, &declare("deploy", &["solar", "lunar"], "dl_slot"));
    install_status(
        &db,
        "deploy",
        &[(
            10,
            Frame {
                state: SequenceChannelState::Stopped,
                loaded: 1,
                accepts: SequenceAccepts::START | SequenceAccepts::RESET,
                artifact: "libdeploy.so",
                message: "refused: the channel is running",
                message_at: 7,
                elapsed: 12.5,
                abort_elapsed: 0.0,
            },
        )],
    );
    let reader = acquire(cx, "deploy");
    cx.update(|cx| {
        let held = reader.read(cx);
        assert!(held.live());
        let channel = held.channel().unwrap();
        assert_eq!(channel.system.as_ref(), "dl_slot");
        let status = held.status();
        assert_eq!(status.state, Some(SequenceChannelState::Stopped));
        assert_eq!(status.loaded, Some(1));
        assert_eq!(channel.sequence(status.loaded).as_deref(), Some("lunar"));
        assert!(status.allows(SequenceAccepts::START));
        assert!(status.allows(SequenceAccepts::RESET));
        assert!(!status.allows(SequenceAccepts::STOP));
        assert_eq!(
            status.message.as_deref(),
            Some("refused: the channel is running")
        );
        assert_eq!(status.message_at, Some(Timestamp(7)));
        assert_eq!(status.artifact.as_deref(), Some("libdeploy.so"));
        assert_eq!(status.elapsed, Some(12.5));
        assert_eq!(status.abort_elapsed, Some(0.0));
        let again = ChannelStatusReader::acquire("deploy".into(), cx).unwrap();
        assert_eq!(
            again.entity_id(),
            reader.entity_id(),
            "one reader per channel"
        );
    });
}

#[gpui::test]
fn nothing_loaded_reads_as_none(cx: &mut TestAppContext) {
    let (db, _temp) = app(cx, &declare("deploy", &["solar"], "dl_slot"));
    install_status(&db, "deploy", &[(10, Frame::default())]);
    let reader = acquire(cx, "deploy");
    cx.update(|cx| {
        let status = reader.read(cx).status();
        assert_eq!(status.state, Some(SequenceChannelState::Unloaded));
        assert_eq!(status.loaded, None);
        assert_eq!(status.accepts, SequenceAccepts(0));
        // Before anything happens the message is empty and stamped zero.
        assert_eq!(status.message_at, None);
        assert_eq!(status.message.as_deref(), Some(""));
    });
}

#[gpui::test]
fn the_reader_follows_the_scrub_time(cx: &mut TestAppContext) {
    let (db, _temp) = app(cx, &declare("deploy", &["solar"], "dl_slot"));
    let frame = |state| Frame {
        state,
        ..Frame::default()
    };
    install_status(
        &db,
        "deploy",
        &[
            (10, frame(SequenceChannelState::Running)),
            (20, frame(SequenceChannelState::Completed)),
        ],
    );
    let reader = acquire(cx, "deploy");
    let state = |cx: &mut TestAppContext| cx.update(|cx| reader.read(cx).status().state);
    assert_eq!(state(cx), Some(SequenceChannelState::Completed));

    cx.update(|cx| {
        crate::temporal::dispatch(TimeAction::Seek(TimeExpr::fixed(Timestamp(15))), cx).unwrap()
    });
    cx.run_until_parked();
    assert_eq!(state(cx), Some(SequenceChannelState::Running));
    assert!(!cx.update(|cx| reader.read(cx).live()));
}

/// History is one entry per distinct `message_at`, so the same text raised
/// twice is two entries while every cycle repeating one message is one.
#[gpui::test]
fn history_has_an_entry_per_raised_message(cx: &mut TestAppContext) {
    use SequenceChannelState::{Running, Stopped};
    let (db, _temp) = app(cx, &declare("deploy", &["solar"], "dl_slot"));
    let frame = |state, message, message_at| Frame {
        state,
        message,
        message_at,
        ..Frame::default()
    };
    let refused = "refused: the channel is running";
    install_status(
        &db,
        "deploy",
        &[
            // Nothing raised yet.
            (1, frame(Stopped, "", 0)),
            (2, frame(Running, "started", 2)),
            (3, frame(Running, "started", 2)),
            (4, frame(Running, refused, 4)),
            (5, frame(Running, refused, 4)),
            (6, frame(Running, refused, 6)),
        ],
    );
    let history = cx.update(|cx| SequenceHistory::global(cx)).unwrap();
    cx.run_until_parked();
    let entries = |cx: &mut TestAppContext| {
        cx.update(|cx| {
            history
                .read(cx)
                .newest_first()
                .into_iter()
                .map(|e| (e.at.0, e.state, e.message.to_string()))
                .collect::<Vec<_>>()
        })
    };
    assert_eq!(
        entries(cx),
        [
            (6, Some(Running), refused.to_string()),
            (4, Some(Running), refused.to_string()),
            (2, Some(Running), "started".to_string()),
        ]
    );

    // Arriving samples extend it; a repeat of the last message adds nothing.
    let generation = cx.update(|cx| history.read(cx).generation());
    install_status(
        &db,
        "deploy",
        &[
            (7, frame(Running, refused, 6)),
            (8, frame(Stopped, "stopped", 8)),
        ],
    );
    cx.run_until_parked();
    let now = entries(cx);
    assert_eq!(now.len(), 4);
    assert_eq!(now[0], (8, Some(Stopped), "stopped".to_string()));
    assert!(cx.update(|cx| history.read(cx).generation()) > generation);
}

#[gpui::test]
fn sequence_flags_mark_each_raised_message(cx: &mut TestAppContext) {
    let (db, _temp) = app(cx, &declare("deploy", &["solar"], "dl_slot"));
    install_status(
        &db,
        "deploy",
        &[
            (
                2,
                Frame {
                    state: SequenceChannelState::Running,
                    message: "started",
                    message_at: 2,
                    ..Frame::default()
                },
            ),
            (
                5,
                Frame {
                    state: SequenceChannelState::Failed,
                    message: "sun lost",
                    message_at: 5,
                    ..Frame::default()
                },
            ),
        ],
    );
    cx.update(crate::plot_events::EventSourceRegistry::init);
    cx.run_until_parked();
    cx.update(|cx| {
        let source = crate::plot_events::EventSourceRegistry::source_for(
            crate::plot_events::EventKindKey::Sequences,
            &db,
            cx,
        )
        .unwrap();
        let events = source.events_in(Timestamp(0)..Timestamp(10), cx);
        let flags: Vec<(i64, &str, &str)> = events
            .iter()
            .map(|e| (e.ts.0, e.label.as_ref(), e.short.as_ref()))
            .collect();
        assert_eq!(
            flags,
            [
                (2, "deploy: started", "started"),
                (5, "deploy: sun lost", "sun lost")
            ]
        );
        assert_eq!(
            events[1].color,
            crate::theme::theme(cx).run_state_color(Some(SequenceChannelState::Failed))
        );
        let index = crate::plot_events::index::snapshot(source.as_ref(), cx);
        assert_eq!(index.events.len(), 2);
    });
}
