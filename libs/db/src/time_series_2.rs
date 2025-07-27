use std::{
    fmt::Debug,
    ops::{Range, RangeInclusive},
    path::{Path, PathBuf},
    sync::Arc,
};

use impeller2::types::Timestamp;
use stellarator::sync::WaitQueue;
use tracing::warn;
use zerocopy::FromBytes;

use crate::{
    Error,
    append_log::AppendLog,
    arc_ring::{AtomicNode, AtomicStack, AtomicStackIter},
    disruptor::ArcAtomic,
};

#[derive(Clone)]
pub struct TimeSeries {
    pub list: Arc<AtomicStack<TimeSeriesNode>>,
    path: PathBuf,
    data_waker: Arc<WaitQueue>,
}

#[derive(Clone)]
pub struct TimeSeriesNode {
    pub index: AppendLog<Timestamp>,
    pub data: AppendLog<u64>,
}

impl Debug for TimeSeriesNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimeSeriesNode").finish()
    }
}

impl TimeSeriesNode {
    pub fn create(
        path: impl AsRef<Path>,
        start_timestamp: Timestamp,
        element_size: u64,
    ) -> Result<Self, Error> {
        const NODE_SIZE: u64 = 1024 * 1024 * 32;
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let index = AppendLog::with_size(NODE_SIZE, path.join("index"), start_timestamp)?;
        let data = AppendLog::with_size(NODE_SIZE, path.join("data"), element_size)?;
        let time_series = Self { index, data };
        Ok(time_series)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let index = AppendLog::open(path.join("index"))?;
        let data = AppendLog::open(path.join("data"))?;
        let time_series = Self { index, data };
        Ok(time_series)
    }

    pub fn push_buf(&self, timestamp: Timestamp, buf: &[u8]) -> Result<(), Error> {
        let len = self.index.len() as usize;

        // check if timestamp is greater than the last timestamp
        // to ensure index is ordered
        if len > 0 {
            let last_timestamp = self
                .index
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

        // write new data to head of data writer
        self.data.write(buf)?;

        // always write index last so we get consistent reads
        self.index.write(&timestamp.to_le_bytes())?;

        Ok(())
    }

    pub fn timestamps(&self) -> &[Timestamp] {
        <[Timestamp]>::ref_from_bytes(self.index.get(..).expect("couldn't get full range"))
            .expect("mmep unaligned")
    }

    pub fn element_size(&self) -> usize {
        *self.data.extra() as usize
    }
}

#[derive(Clone)]
pub struct TimestampRef {
    node: Arc<AtomicNode<TimeSeriesNode>>,
    timestamp: Timestamp,
    index: usize,
}

pub struct TimeSeriesNodeSlice {
    node: Arc<AtomicNode<TimeSeriesNode>>,
    range: RangeInclusive<usize>,
}

impl TimeSeriesNodeSlice {
    pub fn timestamps(&self) -> &[Timestamp] {
        let start: usize = *self.range.start(); //.min(self.node.timestamps().len().saturating_sub(1));
        let mut end = *self.range.end(); //.min(self.node.timestamps().len().saturating_sub(1));
        if end >= self.node.timestamps().len() {
            println!(
                "End index out of bounds {end:?} >= {}",
                self.node.timestamps().len()
            );
            end = self.node.timestamps().len().saturating_sub(1);
        }
        &self.node.timestamps()[start..=end]
    }

    pub fn data(&self) -> &[u8] {
        let element_size = self.node.element_size();
        let data_len = self.node.data.data().len();
        let start = (self.range.start() * element_size).min(data_len);
        let end = (self.range.end().saturating_add(1) * element_size).min(data_len);
        &self.node.data.data()[start..end]
    }
}

pub struct TimeSeriesSlice {
    start: TimestampRef,
    end: TimestampRef,
}

impl TimeSeriesSlice {
    pub fn as_iter(&self) -> impl Iterator<Item = TimeSeriesNodeSlice> + '_ {
        let iter: AtomicStackIter<TimeSeriesNode> =
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
                node.timestamps().len().saturating_sub(1)
            };
            TimeSeriesNodeSlice {
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

impl TimestampRef {
    pub fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    pub fn data(&self) -> &[u8] {
        self.node
            .data
            .get(self.index * self.node.element_size()..(self.index + 1) * self.node.element_size())
            .expect("buffer out of bounds")
    }
}

impl TimeSeries {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            list: Arc::new(AtomicStack::new()),
            path: path.as_ref().to_path_buf(),
            data_waker: Arc::new(WaitQueue::new()),
        })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let list = Arc::new(AtomicStack::new());

        // Scan directory for existing time series nodes
        if path.exists() {
            let entries = std::fs::read_dir(path)?;
            for entry in entries {
                let entry = entry?;
                let node_path = entry.path();
                if node_path.is_dir() {
                    match TimeSeriesNode::open(&node_path) {
                        Ok(node) => {
                            list.push(node);
                        }
                        Err(e) => {
                            warn!(?node_path, ?e, "failed to open time series node");
                        }
                    }
                }
            }
        }

        Ok(Self {
            list,
            path: path.to_path_buf(),
            data_waker: Arc::new(WaitQueue::new()),
        })
    }

    pub fn start_timestamp(&self) -> Option<Timestamp> {
        self.list
            .iter()
            .filter_map(|node| node.timestamps().first().copied())
            .min()
    }

    pub fn push_buf(&self, timestamp: Timestamp, buf: &[u8]) -> Result<(), Error> {
        // TODO(sphw): have a lock for push_buf
        //  Or not, really there is just going to be one writer, maybe we do the split thing?
        // TODO(sphw): prevent time tavel
        loop {
            if self.try_push_buf(timestamp, buf)? {
                break;
            }
            let _ = self.list.try_push(TimeSeriesNode::create(
                self.path.join(timestamp.0.to_string()),
                timestamp,
                buf.len() as u64,
            )?);
        }
        self.data_waker.wake_all();
        Ok(())
    }

    fn try_push_buf(&self, timestamp: Timestamp, buf: &[u8]) -> Result<bool, Error> {
        let Some(head) = self.list.head() else {
            return Ok(false);
        };
        match head.data.write(buf) {
            Ok(_) => {}
            Err(Error::MapOverflow) => return Ok(false),
            Err(err) => return Err(err),
        };
        head.index.write(&timestamp.to_le_bytes())?;
        Ok(true)
    }

    pub fn get(&self, timestamp: Timestamp) -> Option<TimestampRef> {
        for node in self.list.iter() {
            let timestamps = node.timestamps();
            let Ok(index) = timestamps.binary_search(&timestamp) else {
                continue;
            };
            let element_size = node.element_size();
            let i = index * element_size;
            if node.data.get(i..i + element_size).is_none() {
                continue;
            }
            return Some(TimestampRef {
                node,
                timestamp,
                index,
            });
        }
        None
    }

    pub fn binary_search_nearest(
        &self,
        timestamp: Timestamp,
        inclusive: bool,
    ) -> Option<TimestampRef> {
        // three different cases per node
        // 1. Timestamp > node.end
        //   a. prev_node.end.dist(timestamp) < node.start.dist(timestamp) then prev_node.end
        //   b. prev_node.end.dist(timestamp) > node.start.dist(timestamp) then node.start
        // 2. Timestamp < node.start
        //  set prev_node  and skip node
        // 3. Timestamp is within node.start..node.end
        //  node.timestamps.bst(timestamp)

        let mut prev_node: Option<TimestampRef> = None;
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
                return Some(TimestampRef {
                    timestamp: *start,
                    index: node.timestamps().len().saturating_sub(inclusive as usize),
                    node,
                });
            }
            if timestamp.0 < start.0 {
                prev_node = Some(TimestampRef {
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
                return Some(TimestampRef {
                    timestamp,
                    node,
                    index,
                });
            }
        }

        prev_node
    }

    pub fn get_range(&self, range: Range<Timestamp>) -> Option<TimeSeriesSlice> {
        let start = self.binary_search_nearest(range.start, false)?;
        let end = self.binary_search_nearest(range.end, true)?;
        Some(TimeSeriesSlice { start, end })
    }

    pub async fn wait(&self) {
        let _ = self.data_waker.wait().await;
    }

    pub fn waiter(&self) -> Arc<WaitQueue> {
        self.data_waker.clone()
    }

    pub fn latest(&self) -> Option<TimestampRef> {
        let head = self.list.head()?;
        let index = head.timestamps().len().checked_sub(1)?;
        Some(TimestampRef {
            timestamp: head.timestamps()[index],
            node: head,
            index,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.list.head().is_none()
    }
}
