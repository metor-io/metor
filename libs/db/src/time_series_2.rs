use std::{ops::Range, path::Path, sync::Arc};

use impeller2::types::Timestamp;
use stellarator::sync::WaitQueue;
use tracing::warn;
use zerocopy::FromBytes;

use crate::{
    Error,
    append_log::AppendLog,
    arc_ring::{ArcProj, ArcProjExt, AtomicList, AtomicNode, AtomicRing},
};

#[derive(Clone)]
pub struct TimeSeries {
    list: AtomicList<TimeSeriesNode>,
    data_waker: Arc<WaitQueue>,
}

#[derive(Clone)]
pub struct TimeSeriesNode {
    index: AppendLog<Timestamp>,
    data: AppendLog<u64>,
}

impl TimeSeriesNode {
    pub fn create(
        path: impl AsRef<Path>,
        start_timestamp: Timestamp,
        element_size: u64,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let index = AppendLog::create(path.join("index"), start_timestamp)?;
        let data = AppendLog::create(path.join("data"), element_size)?;
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

impl TimeSeries {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            list: AtomicList::new(),
            data_waker: Arc::new(WaitQueue::new()),
        })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        todo!()
    }

    pub fn start_timestamp(&self) -> Timestamp {
        todo!()
        // let index_ts = *self.index.extra();
        // match self.timestamps().next().and_then(|t| t.first()) {
        //     Some(first_ts) => index_ts.min(*first_ts),
        //     None => index_ts,
        // }
    }

    fn timestamps(&self) -> impl Iterator<Item = ArcProj<AtomicNode<TimeSeriesNode>, [Timestamp]>> {
        self.list
            .iter()
            .map(|node| node.proj_fn(|node| node.value().timestamps()))
    }

    pub fn get(
        &self,
        timestamp: Timestamp,
    ) -> Option<
        ArcProj<AtomicNode<TimeSeriesNode>, [u8], impl Fn(&AtomicNode<TimeSeriesNode>) -> &[u8]>,
    > {
        for node in self.list.iter() {
            let timestamps = node.timestamps();
            let index = timestamps.binary_search(&timestamp).ok()?;
            let element_size = node.element_size();
            let i = index * element_size;
            if node.data.get(i..i + element_size).is_none() {
                return None;
            }
            return Some(node.proj(move |node| node.data.get(i..i + element_size).unwrap()));
        }
        None
    }

    pub fn get_nearest(
        &self,
        timestamp: Timestamp,
    ) -> Option<ArcProj<TimeSeriesNode, (Timestamp, &[u8])>> {
        let mut nearest: Option<ArcProj<TimeSeriesNode, (Timestamp, &[u8])>> = None;
        let mut nearest_timestamp: Option<Timestamp> = None;
        for node in self.list.iter() {
            let timestamps =
                <[Timestamp]>::ref_from_bytes(node.index.get(..).expect("couldn't get full range"))
                    .expect("mmep unaligned");
            let index = match timestamps.binary_search(&timestamp) {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            };
            let element_size = node.element_size();
            let guess_timestamp = timestamps.get(index)?;
            let i = index * element_size;
            let _buf = node.data.get(i..i + element_size)?;
            if let Some(t) = &nearest_timestamp {
                if t.0.abs_diff(guess_timestamp.0) > t.0.abs_diff(guess_timestamp.0) {
                    nearest = Some(node.proj(move |node| {
                        (node.timestamps()[index]
                            node.data.get(i..i+element_size).unwrap())
                    }))
                    //nearest = Some((*guess_timestamp, buf));
                }
            } else {
                //nearest = Some((*guess_timestamp, buf));
            }
        }
        nearest
        //     let timestamps =
        //         <[Timestamp]>::ref_from_bytes(self.index.get(..).expect("couldn't get full range"))
        //             .expect("mmep unaligned");
        //     let index = match timestamps.binary_search(&timestamp) {
        //         Ok(i) => i,
        //         Err(i) => i.saturating_sub(1),
        //     };
        //     let element_size = self.element_size();
        //     let timestamp = timestamps.get(index)?;
        //     let i = index * element_size;
        //     let buf = self.data.get(i..i + element_size)?;
        //     Some((*timestamp, buf))
    }

    // pub fn get_range(
    //     &self,
    //     range: Range<Timestamp>,
    // ) -> impl Iterator<Item = (&[Timestamp], &[u8])> {
    //     self.timestamps().filter_map(move |timestamps| {
    //         let start = range.start;
    //         let end = range.end;
    //         let start_index = match timestamps.binary_search(&start) {
    //             Ok(i) => i,
    //             Err(i) => i,
    //         };

    //         let end_index = match timestamps.binary_search(&end) {
    //             Ok(i) => i,
    //             Err(i) => i.saturating_sub(1),
    //         };

    //         let timestamps = timestamps.get(start_index..=end_index)?;
    //         let element_size = self.element_size();
    //         let data = self
    //             .data
    //             .get(start_index * element_size..end_index.saturating_add(1) * element_size)?;

    //         Some((timestamps, data))
    //     })
    // }

    // pub async fn wait(&self) {
    //     let _ = self.data_waker.wait().await;
    // }

    // pub fn waiter(&self) -> Arc<WaitQueue> {
    //     self.data_waker.clone()
    // }

    // pub fn latest(&self) -> Option<(&Timestamp, &[u8])> {
    //     let timestamps =
    //         <[Timestamp]>::ref_from_bytes(self.index.get(..).expect("couldn't get full range"))
    //             .expect("mmep unaligned");
    //     let index = (self.index.len() as usize / size_of::<Timestamp>()).saturating_sub(1);
    //     let element_size = self.element_size();
    //     let i = index * element_size;
    //     let data = self.data.get(i..i + element_size)?;
    //     let timestamp = timestamps.get(index)?;
    //     Some((timestamp, data))
    // }

    // pub(crate) fn data(&self) -> &AppendLog<u64> {
    //     &self.data
    // }

    // pub(crate) fn index(&self) -> &AppendLog<Timestamp> {
    //     &self.index
    // }
}
