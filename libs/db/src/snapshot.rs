//! Independent per-series committed-prefix capture. Each stream may end at a
//! slightly different point; every included record is complete. No writers are
//! paused and pending WAL data is left for subsequent captures.
use crate::{DB, Error, append_log::FrozenLog, manifest::SpanState};
use metor_proto::types::Timestamp;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryGap {
    pub component: u64,
    pub start: i64,
    pub end: i64,
}

pub struct Snapshot {
    pub captured_at: Timestamp,
    pub capture_duration: Duration,
    pub gaps: Vec<HistoryGap>,
    files: Vec<SnapshotFile>,
}
pub struct SnapshotFile {
    pub path: String,
    data: Contents,
}
enum Contents {
    Bytes(Vec<u8>),
    Log(FrozenLog),
}
impl SnapshotFile {
    pub fn parts(&self) -> [&[u8]; 2] {
        match &self.data {
            Contents::Bytes(bytes) => [bytes, &[]],
            Contents::Log(log) => log.parts(),
        }
    }
    pub fn len(&self) -> u64 {
        self.parts().iter().map(|part| part.len() as u64).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn bytes(path: String, bytes: Vec<u8>) -> Self {
        Self {
            path,
            data: Contents::Bytes(bytes),
        }
    }
    fn log(path: String, log: FrozenLog) -> Self {
        Self {
            path,
            data: Contents::Log(log),
        }
    }
}
impl Snapshot {
    pub fn files(&self) -> &[SnapshotFile] {
        &self.files
    }
}
impl DB {
    /// Known unavailable history; snapshots include local bytes only.
    pub fn snapshot_gaps(&self) -> Vec<HistoryGap> {
        self.with_state(|state| {
            state
                .components
                .iter()
                .flat_map(|(id, component)| {
                    component
                        .time_series
                        .manifest()
                        .spans
                        .iter()
                        .filter(|span| span.state != SpanState::Resident)
                        .map(|span| HistoryGap {
                            component: id.0,
                            start: span.seal.start_ts.0,
                            end: span.cover_end.0,
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        })
    }

    /// Pin current node sets and capture a committed length independently for
    /// each node. Timestamp indexes commit after values/payloads, so their
    /// lengths define complete records even if a writer is midway through its
    /// next append. Never flush a live WAL as part of capture.
    pub fn snapshot(&self) -> Result<Snapshot, Error> {
        if !cfg!(target_endian = "little") {
            return Err(std::io::Error::other("Snapshots require a little-endian host").into());
        }
        let started = Instant::now();
        let captured_at = Timestamp::now();
        let (config, components, logs) = self.with_state(|state| {
            (
                state.db_config.clone(),
                state
                    .components
                    .iter()
                    .map(|(id, component)| {
                        (
                            *id,
                            component.clone(),
                            state.component_metadata.get(id).cloned(),
                        )
                    })
                    .collect::<Vec<_>>(),
                state
                    .msg_logs
                    .iter()
                    .map(|(id, log)| (*id, log.clone()))
                    .collect::<Vec<_>>(),
            )
        });
        let mut files = vec![SnapshotFile::bytes(
            "db/db_state".into(),
            postcard::to_allocvec(&config)?,
        )];
        let mut gaps = Vec::new();
        for (id, component, metadata) in components {
            let prefix = format!("db/{}", id.0);
            files.push(SnapshotFile::bytes(
                format!("{prefix}/schema"),
                postcard::to_allocvec(&component.schema)?,
            ));
            if let Some(metadata) = metadata {
                files.push(SnapshotFile::bytes(
                    format!("{prefix}/metadata"),
                    postcard::to_allocvec(&metadata)?,
                ));
            }
            let (manifest, nodes) = component.time_series.snapshot_nodes();
            files.push(SnapshotFile::bytes(
                format!("{prefix}/manifest"),
                manifest.snapshot_bytes()?,
            ));
            gaps.extend(
                manifest
                    .spans
                    .iter()
                    .filter(|s| s.state != SpanState::Resident)
                    .map(|s| HistoryGap {
                        component: id.0,
                        start: s.seal.start_ts.0,
                        end: s.cover_end.0,
                    }),
            );
            for node in nodes {
                let index_len = node.index.len() as usize;
                if index_len == 0 {
                    continue;
                }
                let count = index_len / 8;
                let data_len = count
                    .checked_mul(component.schema.size())
                    .ok_or(Error::MapOverflow)?;
                let start = node.timestamps()[0];
                let path = format!("{prefix}/{}", start.0);
                files.push(SnapshotFile::log(
                    format!("{path}/index"),
                    node.index.freeze_prefix(index_len)?,
                ));
                files.push(SnapshotFile::log(
                    format!("{path}/data"),
                    node.data.freeze_prefix(data_len)?,
                ));
                if let Some(span) = manifest
                    .span(start)
                    .filter(|s| s.state == SpanState::Resident)
                {
                    files.push(SnapshotFile::bytes(
                        format!("{path}/seal"),
                        postcard::to_allocvec(&span.seal)?,
                    ));
                }
            }
        }
        for (id, log) in logs {
            let prefix = format!("db/msgs/{}", u16::from_le_bytes(id));
            if let Some(metadata) = log.metadata() {
                files.push(SnapshotFile::bytes(
                    format!("{prefix}/metadata"),
                    postcard::to_allocvec(metadata)?,
                ));
            }
            for node in log.list.iter() {
                let timestamp_len = node.timestamps.len() as usize;
                if timestamp_len == 0 {
                    continue;
                }
                let count = timestamp_len / 8;
                let offsets = &node.bufs.bufs()[..count];
                // Payloads append in offset order. Trailing inline messages
                // consume no payload bytes, so find the last external buffer.
                let data_len = offsets
                    .iter()
                    .rev()
                    .find(|buf| buf.len > 12)
                    .map(|buf| {
                        // The timestamp's release commit publishes this descriptor.
                        (unsafe { buf.data.offset.offset as usize }) + buf.len as usize
                    })
                    .unwrap_or(0);
                let path = format!("{prefix}/{}", node.timestamps()[0].0);
                files.push(SnapshotFile::log(
                    format!("{path}/timestamps"),
                    node.timestamps.freeze_prefix(timestamp_len)?,
                ));
                files.push(SnapshotFile::log(
                    format!("{path}/offsets"),
                    node.bufs
                        .offsets
                        .freeze_prefix(count * size_of::<metor_proto::buf::UmbraBuf>())?,
                ));
                files.push(SnapshotFile::log(
                    format!("{path}/data_log"),
                    node.bufs.data_log.freeze_prefix(data_len)?,
                ));
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Snapshot {
            files,
            gaps,
            captured_at,
            capture_duration: started.elapsed(),
        })
    }
}

#[cfg(test)]
mod tests;
