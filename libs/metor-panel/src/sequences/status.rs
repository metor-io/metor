//! A channel's state as its components carry it at the selected time.
//!
//! The control system telemeters each channel's state as components every
//! cycle, writes its message component only when something happens, and the
//! registry names them all, so reading a channel is reading
//! those components the way any instrument does: through
//! [`crate::temporal::samples`], live or at a scrub point. One
//! [`ChannelStatusReader`] per channel is shared by every view showing it,
//! and follows the registry when it renames a component.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    App, AppContext, Context, Entity, Global, SharedString, Subscription, Task, WeakEntity,
};
use metor_db::DB;
use metor_proto::types::{ComponentId, ComponentView, ElementValue, Timestamp};
use metor_proto_wkt::{SequenceAccepts, SequenceChannelState};

use super::{Channel, ChannelComponents, SequenceStore};
use crate::views::binding::{StreamUpdate, spawn_seeded_stream, spawn_stamped_stream};

/// A channel's state, decoded from its components at the selected time.
/// Each field is `None` (or empty) while its component has no reading.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChannelStatus {
    pub state: Option<SequenceChannelState>,
    /// Index into the channel's `available`; `None` when nothing is loaded.
    pub loaded: Option<u32>,
    pub artifact: Option<SharedString>,
    /// Exactly the commands the target would accept at the selected time.
    pub accepts: SequenceAccepts,
    /// The latest transition, refusal or end; never empty.
    pub message: Option<SharedString>,
    /// When `message` was raised: its sample's stamp.
    pub message_at: Option<Timestamp>,
    pub elapsed: Option<f64>,
    pub abort_elapsed: Option<f64>,
}

/// The shared reader for one channel's components.
pub struct ChannelStatusReader {
    name: SharedString,
    /// The channel's registry entry, while the registry declares it.
    channel: Option<Channel>,
    status: ChannelStatus,
    /// Whether the timeline follows live data. Scrubbed, the status is the
    /// past's and no command applies to it.
    live: bool,
    db: Arc<DB>,
    bound: Option<ChannelComponents>,
    _streams: Vec<Task<()>>,
    _registry: Subscription,
    _clock: Subscription,
}

#[derive(Default)]
struct Readers(HashMap<SharedString, WeakEntity<ChannelStatusReader>>);
impl Global for Readers {}

impl ChannelStatusReader {
    /// The reader for the channel named `name`, shared while any holder keeps
    /// it. `None` without a sequence store.
    pub fn acquire(name: SharedString, cx: &mut App) -> Option<Entity<Self>> {
        if let Some(existing) = cx
            .try_global::<Readers>()
            .and_then(|r| r.0.get(&name))
            .and_then(WeakEntity::upgrade)
        {
            return Some(existing);
        }
        let store = super::try_global(cx)?;
        let db = store.read(cx).db().clone();
        let clock = crate::temporal::TemporalController::init(db.clone(), cx);
        let reader = cx.new(|cx| {
            let mut this = Self {
                name: name.clone(),
                channel: None,
                status: ChannelStatus::default(),
                live: crate::temporal::is_live(cx),
                db,
                bound: None,
                _streams: Vec::new(),
                _registry: cx.observe(&store, |this: &mut Self, store, cx| this.follow(&store, cx)),
                _clock: cx.observe(&clock, |this: &mut Self, _, cx| {
                    let live = crate::temporal::is_live(cx);
                    if live != this.live {
                        this.live = live;
                        cx.notify();
                    }
                }),
            };
            this.follow(&store, cx);
            this
        });
        cx.default_global::<Readers>()
            .0
            .insert(name, reader.downgrade());
        Some(reader)
    }

    pub fn name(&self) -> &SharedString {
        &self.name
    }

    /// The channel's registry entry; `None` while the registry doesn't
    /// declare it.
    pub fn channel(&self) -> Option<&Channel> {
        self.channel.as_ref()
    }

    pub fn status(&self) -> &ChannelStatus {
        &self.status
    }

    pub fn live(&self) -> bool {
        self.live
    }

    /// The commands the channel would take now: what it accepts, or nothing
    /// while scrubbed, since nothing is commandable from the past.
    pub fn accepts(&self) -> SequenceAccepts {
        if self.live {
            self.status.accepts
        } else {
            SequenceAccepts::default()
        }
    }

    /// Pick up the registry's entry for this channel, restreaming only when
    /// it names different components.
    fn follow(&mut self, store: &Entity<SequenceStore>, cx: &mut Context<Self>) {
        let channel = store.read(cx).state().channel(&self.name).cloned();
        let components = channel.as_ref().map(|c| c.components);
        if channel != self.channel {
            self.channel = channel;
            cx.notify();
        }
        if components == self.bound {
            return;
        }
        self.bound = components;
        self.status = ChannelStatus::default();
        let Some(c) = components else {
            self._streams.clear();
            return;
        };
        let db = self.db.clone();
        self._streams = [
            field(&db, c.state, code, cx, |s, v| {
                s.state = v.and_then(SequenceChannelState::from_code)
            }),
            field(&db, c.loaded, code, cx, |s, v| {
                s.loaded = v.filter(|&i| i != u32::MAX)
            }),
            field(&db, c.artifact, text, cx, |s, v| s.artifact = v),
            field(&db, c.accepts, code, cx, |s, v| {
                s.accepts = SequenceAccepts(v.unwrap_or(0))
            }),
            message(&db, c.message, cx),
            field(&db, c.elapsed, seconds, cx, |s, v| s.elapsed = v),
            field(&db, c.abort_elapsed, seconds, cx, |s, v| {
                s.abort_elapsed = v
            }),
        ]
        .into_iter()
        .flatten()
        .collect();
    }
}

/// Stream one component into the field `set` writes, decoded by `decode`.
fn field<V: Send + 'static>(
    db: &Arc<DB>,
    id: Option<ComponentId>,
    decode: fn(ComponentView<'_>) -> Option<V>,
    cx: &mut Context<ChannelStatusReader>,
    set: fn(&mut ChannelStatus, Option<V>),
) -> Option<Task<()>> {
    Some(spawn_seeded_stream(
        db.clone(),
        id?,
        cx,
        move |_, _| ((), decode),
        move |this: &mut ChannelStatusReader, update, cx| {
            let value = match update {
                StreamUpdate::Value(value) => Some(value),
                StreamUpdate::Unavailable => None,
                StreamUpdate::Ready(()) | StreamUpdate::Stale => return,
            };
            set(&mut this.status, value);
            cx.notify();
        },
    ))
}

/// Stream the message component: each sample is one message, stamped when it
/// was raised.
fn message(
    db: &Arc<DB>,
    id: Option<ComponentId>,
    cx: &mut Context<ChannelStatusReader>,
) -> Option<Task<()>> {
    Some(spawn_stamped_stream(
        db.clone(),
        id?,
        cx,
        |_, _| {
            ((), |at, view| {
                Some((text(view).filter(|t| !t.is_empty())?, at))
            })
        },
        |this: &mut ChannelStatusReader, update, cx| {
            let (message, at) = match update {
                StreamUpdate::Value((message, at)) => (Some(message), Some(at)),
                StreamUpdate::Unavailable => (None, None),
                StreamUpdate::Ready(()) | StreamUpdate::Stale => return,
            };
            this.status.message = message;
            this.status.message_at = at;
            cx.notify();
        },
    ))
}

fn leading(view: &ComponentView<'_>) -> Option<ElementValue> {
    view.iter().next()
}

/// A `u32` code from any numeric element type.
pub(crate) fn code(view: ComponentView<'_>) -> Option<u32> {
    let value = leading(&view)?.as_f64();
    (value.is_finite() && value >= 0.0 && value <= u32::MAX as f64).then_some(value as u32)
}

fn seconds(view: ComponentView<'_>) -> Option<f64> {
    leading(&view).map(|v| v.as_f64())
}

/// Fixed-size text: a `U8` tensor, NUL-terminated.
pub(crate) fn text(view: ComponentView<'_>) -> Option<SharedString> {
    let ComponentView::U8(array) = view else {
        return None;
    };
    let buf = array.buf();
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..len]).into_owned().into())
}

/// A view's readers for a list of channels, kept matched to the list and
/// observed so the view repaints with any of them.
#[derive(Default)]
pub struct ChannelReaders {
    readers: Vec<(Entity<ChannelStatusReader>, Subscription)>,
}

impl ChannelReaders {
    /// Hold a reader for each of `names`, in order, keeping the ones already
    /// held.
    pub fn sync<E: 'static>(&mut self, names: &[SharedString], cx: &mut Context<E>) {
        if self.readers.len() == names.len()
            && self
                .readers
                .iter()
                .zip(names)
                .all(|((reader, _), name)| reader.read(cx).name() == name)
        {
            return;
        }
        let mut old = std::mem::take(&mut self.readers);
        self.readers = names
            .iter()
            .filter_map(
                |name| match old.iter().position(|(r, _)| r.read(cx).name() == name) {
                    Some(ix) => Some(old.swap_remove(ix)),
                    None => {
                        let reader = ChannelStatusReader::acquire(name.clone(), cx)?;
                        let observed = cx.observe(&reader, |_, _, cx| cx.notify());
                        Some((reader, observed))
                    }
                },
            )
            .collect();
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entity<ChannelStatusReader>> {
        self.readers.iter().map(|(reader, _)| reader)
    }
}
