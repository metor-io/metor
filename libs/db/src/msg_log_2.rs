use std::{
    fmt::Debug,
    future::Future,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use impeller2::{buf::UmbraBuf, types::Timestamp};
use impeller2_wkt::MsgMetadata;
use stellarator::sync::WaitQueue;
use tracing::warn;
use zerocopy::{FromBytes, IntoBytes};

use crate::{
    Error, MetadataExt,
    append_log::AppendLog,
    arc_ring::{AtomicNode, AtomicStack, AtomicStackIter},
    disruptor::{ArcAtomic, Disruptor},
};

#[derive(Clone)]
pub struct MsgLog {
    pub list: Arc<AtomicStack<MsgLogNode>>,
    path: PathBuf,
    data_waker: Arc<WaitQueue>,
    metadata: Option<MsgMetadata>,
    wal: Disruptor,
}

#[derive(Clone)]
pub struct MsgLogNode {
    pub timestamps: AppendLog<()>,
    pub bufs: BufLog,
}

impl Debug for MsgLogNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsgLogNode").finish()
    }
}

impl MsgLogNode {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let timestamps = AppendLog::create(path.join("timestamps"), ())?;
        let offsets = AppendLog::create(path.join("offsets"), ())?;
        let data_log = AppendLog::create(path.join("data_log"), ())?;
        let node = Self {
            timestamps,
            bufs: BufLog { offsets, data_log },
        };
        Ok(node)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let timestamps = AppendLog::open(path.join("timestamps"))?;
        let offsets = AppendLog::open(path.join("offsets"))?;
        let data_log = AppendLog::open(path.join("data_log"))?;
        let node = Self {
            timestamps,
            bufs: BufLog { offsets, data_log },
        };
        Ok(node)
    }

    pub fn push(&self, timestamp: Timestamp, msg: &[u8]) -> Result<(), Error> {
        // Check if timestamp is greater than the last timestamp to ensure ordering
        let len = self.timestamps.len() as usize;
        if len > 0 {
            let last_timestamp = self
                .timestamps
                .get(len - size_of::<i64>()..len)
                .expect("couldn't find last timestamp");
            let last_timestamp = Timestamp::from_le_bytes(
                last_timestamp
                    .try_into()
                    .expect("last_timestamp was wrong size"),
            );
            if last_timestamp > timestamp {
                warn!(?last_timestamp, ?timestamp, "time travel");
                return Err(Error::TimeTravel);
            }
        }

        // Insert message into buffer log
        self.bufs.insert_msg(msg)?;

        // Always write timestamp last for consistent reads
        self.timestamps.write(&timestamp.to_le_bytes())?;

        Ok(())
    }

    pub fn timestamps(&self) -> &[Timestamp] {
        <[Timestamp]>::ref_from_bytes(self.timestamps.get(..).expect("couldn't get full range"))
            .expect("mmep unaligned")
    }

    pub fn msg_count(&self) -> usize {
        self.timestamps.len() as usize / size_of::<Timestamp>()
    }
}

#[derive(Clone)]
pub struct BufLog {
    offsets: AppendLog<()>,
    data_log: AppendLog<()>,
}

impl BufLog {
    pub fn bufs(&self) -> &[UmbraBuf] {
        <[UmbraBuf]>::ref_from_bytes(self.offsets.data()).expect("offsets buf invalid")
    }

    pub fn get_msg(&self, index: usize) -> Option<&[u8]> {
        let buf = self.bufs().get(index)?;
        let data = match buf.len as usize {
            len @ ..=12 => unsafe { &buf.data.inline[..len] },
            len => {
                let offset = unsafe { buf.data.offset.offset } as usize;
                self.data_log.get(offset..offset + len)?
            }
        };
        Some(data)
    }

    pub fn insert_msg(&self, msg: &[u8]) -> Result<(), Error> {
        let len = msg.len() as u32;
        let buf = if len > 12 {
            let prefix = msg[..4].try_into().expect("trivial cast failed");
            let offset = self.data_log.write(msg)?;
            UmbraBuf::with_offset(len, prefix, offset as u32)
        } else {
            let mut inline = [0u8; 12];
            inline[..msg.len()].copy_from_slice(msg);
            UmbraBuf::with_inline(len, inline)
        };
        self.offsets.write(buf.as_bytes())?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct MsgRef {
    node: Arc<AtomicNode<MsgLogNode>>,
    timestamp: Timestamp,
    index: usize,
}

pub struct MsgLogNodeSlice {
    node: Arc<AtomicNode<MsgLogNode>>,
    range: std::ops::RangeInclusive<usize>,
}

impl MsgLogNodeSlice {
    pub fn timestamps(&self) -> &[Timestamp] {
        let start: usize = *self.range.start();
        let mut end = *self.range.end();
        if end >= self.node.timestamps().len() {
            end = self.node.timestamps().len().saturating_sub(1);
        }
        &self.node.timestamps()[start..=end]
    }

    pub fn msgs(&self) -> impl Iterator<Item = (Timestamp, &[u8])> + '_ {
        let start = *self.range.start();
        let end = (*self.range.end()).min(self.node.timestamps().len().saturating_sub(1));

        (start..=end).filter_map(move |i| {
            let timestamp = self.node.timestamps().get(i)?;
            let msg = self.node.bufs.get_msg(i)?;
            Some((*timestamp, msg))
        })
    }
}

pub struct MsgLogSlice {
    start: MsgRef,
    end: MsgRef,
}

impl MsgLogSlice {
    pub fn as_iter(&self) -> impl Iterator<Item = MsgLogNodeSlice> + '_ {
        let iter: AtomicStackIter<MsgLogNode> =
            AtomicStackIter::new(ArcAtomic::from(self.start.node.clone()));
        iter.map(move |node| {
            let start = if Arc::ptr_eq(&node, &self.start.node) {
                self.start.index
            } else {
                0
            };
            let end = if Arc::ptr_eq(&node, &self.end.node) {
                self.end.index
            } else {
                node.msg_count().saturating_sub(1)
            };
            MsgLogNodeSlice {
                range: start..=end,
                node,
            }
        })
    }

    pub fn len(&self) -> usize {
        self.as_iter().map(|node| node.timestamps().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl MsgRef {
    pub fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    pub fn data(&self) -> Option<&[u8]> {
        self.node.bufs.get_msg(self.index)
    }
}

impl MsgLog {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let this = Self {
            list: Arc::new(AtomicStack::new()),
            path: path.to_path_buf(),
            data_waker: Arc::new(WaitQueue::new()),
            metadata: None,
            wal: Disruptor::new(1024 * 1024), // 1MB WAL buffer
        };
        stellarator::spawn(this.clone().persist());
        Ok(this)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let list = Arc::new(AtomicStack::new());
        let metadata_path = path.join("metadata");
        let metadata = if metadata_path.exists() {
            Some(MsgMetadata::read(metadata_path)?)
        } else {
            None
        };

        let entries = std::fs::read_dir(path)?;
        for entry in entries {
            let entry = entry?;
            let node_path = entry.path();
            if node_path.is_dir() && node_path.file_name().unwrap_or_default() != "metadata" {
                match MsgLogNode::open(&node_path) {
                    Ok(node) => {
                        list.push(node);
                    }
                    Err(e) => {
                        warn!(?node_path, ?e, "failed to open msg log node");
                    }
                }
            }
        }

        let this = Self {
            list,
            path: path.to_path_buf(),
            data_waker: Arc::new(WaitQueue::new()),
            metadata,
            wal: Disruptor::new(1024 * 1024), // 1MB WAL buffer
        };
        stellarator::spawn(this.clone().persist());
        Ok(this)
    }

    pub fn push(&self, timestamp: Timestamp, msg: &[u8]) -> Result<(), Error> {
        let grant_size = size_of::<Timestamp>() + size_of::<u32>() + msg.len();

        let Ok(mut grant) = self.wal.try_grant(grant_size) else {
            // WAL is full, could handle this differently (wait, error, etc.)
            return Err(Error::MapOverflow);
        };

        let mut offset = 0;

        // Write timestamp
        grant[offset..offset + size_of::<Timestamp>()].copy_from_slice(timestamp.as_bytes());
        offset += size_of::<Timestamp>();

        // Write message length
        let msg_len = msg.len() as u32;
        grant[offset..offset + size_of::<u32>()].copy_from_slice(&msg_len.to_le_bytes());
        offset += size_of::<u32>();

        // Write message data
        grant[offset..offset + msg.len()].copy_from_slice(msg);

        drop(grant);
        self.data_waker.wake_all();
        Ok(())
    }

    fn try_push(&self, timestamp: Timestamp, msg: &[u8]) -> Result<bool, Error> {
        let Some(head) = self.list.head() else {
            return Ok(false);
        };
        match head.bufs.insert_msg(msg) {
            Ok(_) => {}
            Err(Error::MapOverflow) => return Ok(false),
            Err(err) => return Err(err),
        };
        head.timestamps.write(&timestamp.to_le_bytes())?;
        Ok(true)
    }

    pub fn get(&self, timestamp: Timestamp) -> Option<MsgRef> {
        for node in self.list.iter() {
            let timestamps = node.timestamps();
            let Ok(index) = timestamps.binary_search(&timestamp) else {
                continue;
            };
            if node.bufs.get_msg(index).is_none() {
                continue;
            }
            return Some(MsgRef {
                node,
                timestamp,
                index,
            });
        }
        None
    }

    pub fn binary_search_nearest(&self, timestamp: Timestamp, inclusive: bool) -> Option<MsgRef> {
        let mut prev_node: Option<MsgRef> = None;
        for node in self.list.iter() {
            let timestamps = node.timestamps();
            let start = timestamps.first()?;
            let end = timestamps.last()?;

            if timestamp.0 > end.0 {
                if let Some(prev) = &prev_node {
                    if prev.timestamp.0.abs_diff(timestamp.0) < start.0.abs_diff(timestamp.0) {
                        return prev_node;
                    }
                }
                return Some(MsgRef {
                    timestamp: *start,
                    index: node.msg_count().saturating_sub(inclusive as usize),
                    node,
                });
            }
            if timestamp.0 < start.0 {
                prev_node = Some(MsgRef {
                    timestamp: *end,
                    index: 0,
                    node,
                });
                continue;
            }
            if timestamp.0 >= start.0 && timestamp.0 <= end.0 {
                let index = match timestamps.binary_search(&timestamp) {
                    Ok(i) => i,
                    Err(i) => i.saturating_sub(inclusive as usize),
                };
                let timestamp = timestamps[index];
                return Some(MsgRef {
                    timestamp,
                    node,
                    index,
                });
            }
        }

        prev_node
    }

    pub fn get_nearest(&self, timestamp: Timestamp) -> Option<MsgRef> {
        self.binary_search_nearest(timestamp, true)
    }

    pub fn get_range(&self, range: Range<Timestamp>) -> Option<MsgLogSlice> {
        let start = self.binary_search_nearest(range.start, false)?;
        let end = self.binary_search_nearest(range.end, true)?;
        Some(MsgLogSlice { start, end })
    }

    pub fn latest(&self) -> Option<MsgRef> {
        let head = self.list.head()?;
        let index = head.msg_count().checked_sub(1)?;
        let timestamp = head.timestamps()[index];
        Some(MsgRef {
            timestamp,
            node: head,
            index,
        })
    }

    pub async fn wait(&self) {
        let _ = self.data_waker.wait().await;
    }

    pub fn waiter(&self) -> Arc<WaitQueue> {
        self.data_waker.clone()
    }

    pub fn set_metadata(&mut self, metadata: MsgMetadata) -> Result<(), Error> {
        let metadata = self.metadata.insert(metadata);
        std::fs::create_dir_all(&self.path)?;
        let metadata_path = self.path.join("metadata");
        metadata.write(&metadata_path)?;
        Ok(())
    }

    pub fn metadata(&self) -> Option<&MsgMetadata> {
        self.metadata.as_ref()
    }

    pub fn is_empty(&self) -> bool {
        self.list.head().is_none()
    }

    pub fn first_timestamp(&self) -> Option<Timestamp> {
        self.list
            .iter()
            .filter_map(|node| node.timestamps().first().copied())
            .min()
    }

    pub fn start_timestamp(&self) -> Option<Timestamp> {
        self.first_timestamp()
    }

    pub fn persist(self) -> impl Future<Output = ()> {
        let mut reader = self.wal.reader();
        async move {
            loop {
                let buf = reader.next().await;
                let mut buf = &buf[..];

                'parse: while buf.len() >= size_of::<Timestamp>() + size_of::<u32>() {
                    // Read timestamp
                    let timestamp_bytes = &buf[..size_of::<Timestamp>()];
                    let timestamp = Timestamp::from_le_bytes(
                        timestamp_bytes
                            .try_into()
                            .expect("timestamp bytes wrong size"),
                    );
                    buf = &buf[size_of::<Timestamp>()..];

                    if buf.len() < size_of::<u32>() {
                        break 'parse;
                    }
                    let msg_len_bytes = &buf[..size_of::<u32>()];
                    let msg_len = u32::from_le_bytes(
                        msg_len_bytes.try_into().expect("msg_len bytes wrong size"),
                    ) as usize;
                    buf = &buf[size_of::<u32>()..];

                    if buf.len() < msg_len {
                        break 'parse;
                    }
                    let msg_data = &buf[..msg_len];
                    buf = &buf[msg_len..];

                    // Persist to actual storage
                    if let Err(err) = self.persist_msg(timestamp, msg_data) {
                        tracing::error!(?err, "failed to persist wal message");
                    }
                }
            }
        }
    }

    fn persist_msg(&self, timestamp: Timestamp, msg: &[u8]) -> Result<(), Error> {
        loop {
            if self.try_push(timestamp, msg)? {
                break;
            }
            let _ = self
                .list
                .try_push(MsgLogNode::create(self.path.join(timestamp.0.to_string()))?);
        }
        Ok(())
    }
}
