use std::{
    fs::{self, File},
    io::Write as _,
    path::Path,
};

use metor_proto::types::Timestamp;
use zerocopy::FromBytes;

use crate::{Error, time_series_2::TimeSeriesNode};

/// The wire-shared seal struct; storage-side behavior lives in
/// [`SealRecordExt`]. A sealed node's files are immutable from sealing on;
/// the record is the unit of trust for transfer and tiering.
pub use metor_proto_wkt::SealRecord;

pub const SEAL_FILE: &str = "seal";

/// Storage-side behavior for [`SealRecord`] (the struct itself lives in
/// `metor-proto-wkt` so manifests can travel over the wire).
pub trait SealRecordExt: Sized {
    /// Summarize `node` as it stands. Returns `None` for an empty node —
    /// there is nothing worth sealing or transferring.
    fn compute(node: &TimeSeriesNode) -> Option<Self>;
    /// True when `node`'s committed bytes match this record exactly.
    fn verify(&self, node: &TimeSeriesNode) -> bool;
    fn read(node_dir: &Path) -> Result<Option<Self>, Error>;
    /// Persist the record atomically; a crash leaves either no seal or a
    /// complete one, never a torn record.
    fn write(&self, node_dir: &Path) -> Result<(), Error>;
}

impl SealRecordExt for SealRecord {
    /// The committed regions are captured once up front so the record is
    /// self-consistent even if invoked while a writer still appends.
    fn compute(node: &TimeSeriesNode) -> Option<Self> {
        let index = node.index.data();
        let data = node.data.data();
        let timestamps = <[Timestamp]>::ref_from_bytes(index).ok()?;
        let start_ts = *timestamps.first()?;
        let end_ts = *timestamps.last()?;
        Some(Self {
            start_ts,
            end_ts,
            count: timestamps.len() as u64,
            index_len: index.len() as u64,
            data_len: data.len() as u64,
            checksum: checksum(index, data),
            element_size: node.element_size() as u64,
        })
    }

    fn verify(&self, node: &TimeSeriesNode) -> bool {
        let index = node.index.data();
        let data = node.data.data();
        index.len() as u64 == self.index_len
            && data.len() as u64 == self.data_len
            && checksum(index, data) == self.checksum
    }

    fn read(node_dir: &Path) -> Result<Option<Self>, Error> {
        match fs::read(node_dir.join(SEAL_FILE)) {
            Ok(buf) => Ok(Some(postcard::from_bytes(&buf)?)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn write(&self, node_dir: &Path) -> Result<(), Error> {
        atomic_write(node_dir, SEAL_FILE, &postcard::to_allocvec(self)?)
    }
}

/// Durably replace `dir/file_name`: tmp file, fsync, rename into place,
/// fsync the directory. Readers see either the old contents or the new,
/// never a torn write.
pub(crate) fn atomic_write(dir: &Path, file_name: &str, bytes: &[u8]) -> Result<(), Error> {
    let tmp = dir.join(format!("{file_name}.tmp"));
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, dir.join(file_name))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Seal `node` in `node_dir`: flush its mmaps so the bytes the checksum
/// covers are durable before the seal claims them, then write the
/// sidecar. Returns `None` for an empty node.
pub fn seal_node(node: &TimeSeriesNode, node_dir: &Path) -> Result<Option<SealRecord>, Error> {
    let Some(record) = SealRecord::compute(node) else {
        return Ok(None);
    };
    node.index.flush()?;
    node.data.flush()?;
    record.write(node_dir)?;
    Ok(Some(record))
}

fn checksum(index: &[u8], data: &[u8]) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = twox_hash::XxHash3_64::with_seed(0);
    hasher.write(index);
    hasher.write(data);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_node(dir: &Path, start: i64, samples: i64) -> TimeSeriesNode {
        let node = TimeSeriesNode::create(dir, Timestamp(start), 8).unwrap();
        for i in 0..samples {
            node.data.write(&(start + i).to_le_bytes()).unwrap();
            node.index.write(&Timestamp(start + i).to_le_bytes()).unwrap();
        }
        node
    }

    #[test]
    fn seal_round_trip_and_verify() {
        let dir = tempfile::tempdir().unwrap();
        let node_dir = dir.path().join("100");
        let node = sample_node(&node_dir, 100, 5);

        let record = seal_node(&node, &node_dir).unwrap().unwrap();
        assert_eq!(record.start_ts, Timestamp(100));
        assert_eq!(record.end_ts, Timestamp(104));
        assert_eq!(record.count, 5);
        assert_eq!(record.element_size, 8);
        assert!(record.verify(&node));

        let read_back = SealRecord::read(&node_dir).unwrap().unwrap();
        assert_eq!(read_back, record);
    }

    #[test]
    fn verify_rejects_modified_node() {
        let dir = tempfile::tempdir().unwrap();
        let node_dir = dir.path().join("100");
        let node = sample_node(&node_dir, 100, 5);
        let record = seal_node(&node, &node_dir).unwrap().unwrap();

        // Append one more sample after sealing; the record must reject it.
        node.data.write(&0u64.to_le_bytes()).unwrap();
        node.index.write(&Timestamp(200).to_le_bytes()).unwrap();
        assert!(!record.verify(&node));
    }

    #[test]
    fn missing_seal_reads_none() {
        let dir = tempfile::tempdir().unwrap();
        let node_dir = dir.path().join("100");
        sample_node(&node_dir, 100, 1);
        assert!(SealRecord::read(&node_dir).unwrap().is_none());
    }

    #[test]
    fn empty_node_does_not_seal() {
        let dir = tempfile::tempdir().unwrap();
        let node_dir = dir.path().join("100");
        let node = TimeSeriesNode::create(&node_dir, Timestamp(100), 8).unwrap();
        assert!(seal_node(&node, &node_dir).unwrap().is_none());
        assert!(SealRecord::read(&node_dir).unwrap().is_none());
    }
}
