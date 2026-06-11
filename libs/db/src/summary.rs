//! Per-node min/max summaries — the data behind envelope rendering.
//!
//! A sealed node gets one `summary` sidecar beside `index`/`data`/`seal`,
//! computed wherever a full node lands (the seal task, hydration install,
//! peer push). Summaries are pure derived data: they never travel through
//! [`crate::store::NodeStore`], are recomputed rather than transferred,
//! and a missing or stale sidecar is recoverable whenever the node is
//! resident. Wide plot views aggregate these bins instead of touching
//! payload bytes, which is what makes year-scale ranges renderable.

use std::path::Path;

use metor_proto::types::PrimType;
use serde::{Deserialize, Serialize};

use crate::{
    ComponentSchema, Error,
    seal::{SealRecord, atomic_write},
    time_series_2::TimeSeriesNode,
};

pub const SUMMARY_FILE: &str = "summary";
const SUMMARY_VERSION: u32 = 1;

/// Per-element bin budget: enough horizontal resolution that a handful of
/// visible nodes still fill a 4k display, small enough that a year of
/// sidecars stays in the tens of megabytes.
const MAX_BINS: u64 = 1024;
const MIN_BINS: u64 = 64;
/// Wide schemas (FFT vectors and the like) trade bin resolution for a
/// bounded sidecar size.
const BIN_BUDGET: u64 = 65_536;

/// Decoded first so rollup builds can read a bounded prefix of the file
/// (`postcard::take_from_bytes`) without touching the bin arrays.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeSummaryHeader {
    pub version: u32,
    /// The seal this summary was computed from; a mismatch means the
    /// sidecar is stale and must be recomputed.
    pub source_checksum: u64,
    pub count: u64,
    pub n_elements: u32,
    pub bins: u32,
    /// Whole-node aggregate per element — the zero-cost rollup entry.
    pub elem_min: Vec<f32>,
    pub elem_max: Vec<f32>,
}

/// Min/max bins over sample-index space with explicit per-bin timestamps:
/// bins never sit empty and bursty rates cannot skew them. Values are f32
/// because every renderer materializes to f32 anyway; non-finite samples
/// are skipped, and a bin with no finite sample stores `(NaN, NaN)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeSummary {
    pub header: NodeSummaryHeader,
    /// Midpoint sample timestamp per bin, len = `header.bins`.
    pub ts: Vec<i64>,
    /// Element-major: `[e * bins + b]`.
    pub mins: Vec<f32>,
    pub maxs: Vec<f32>,
}

impl NodeSummary {
    /// Summarize a fully-written node. One pass over the payload — the
    /// same cost class as the seal checksum, so it belongs on the same
    /// background tasks and nowhere near ingest. `None` for empty nodes.
    pub fn compute(
        node: &TimeSeriesNode,
        schema: &ComponentSchema,
        seal: &SealRecord,
    ) -> Option<Self> {
        let timestamps = node.timestamps();
        let count = timestamps.len() as u64;
        if count == 0 {
            return None;
        }
        let n_elements = schema.dim.iter().product::<usize>().max(1) as u64;
        let bins = (BIN_BUDGET / n_elements).clamp(MIN_BINS, MAX_BINS).min(count);

        let data = node.data.data();
        let prim = schema.prim_type;
        let prim_size = prim.size();
        let element_size = schema.size();

        let (bins, n_elements, count_us) = (bins as usize, n_elements as usize, count as usize);
        let mut ts = Vec::with_capacity(bins);
        let mut mins = vec![f32::NAN; n_elements * bins];
        let mut maxs = vec![f32::NAN; n_elements * bins];
        let mut elem_min = vec![f32::NAN; n_elements];
        let mut elem_max = vec![f32::NAN; n_elements];

        for b in 0..bins {
            let lo = b * count_us / bins;
            let hi = ((b + 1) * count_us / bins).max(lo + 1);
            ts.push(timestamps[(lo + hi - 1) / 2].0);
            for e in 0..n_elements {
                let mut min = f64::INFINITY;
                let mut max = f64::NEG_INFINITY;
                for s in lo..hi {
                    let offset = s * element_size + e * prim_size;
                    let Some(bytes) = data.get(offset..offset + prim_size) else {
                        continue;
                    };
                    let v = read_f64(prim, bytes);
                    if v.is_finite() {
                        min = min.min(v);
                        max = max.max(v);
                    }
                }
                if min <= max {
                    let slot = e * bins + b;
                    mins[slot] = min as f32;
                    maxs[slot] = max as f32;
                    elem_min[e] = elem_min[e].min(min as f32);
                    elem_max[e] = elem_max[e].max(max as f32);
                }
            }
        }

        Some(Self {
            header: NodeSummaryHeader {
                version: SUMMARY_VERSION,
                source_checksum: seal.checksum,
                count,
                n_elements: n_elements as u32,
                bins: bins as u32,
                elem_min,
                elem_max,
            },
            ts,
            mins,
            maxs,
        })
    }

    pub fn write(&self, node_dir: &Path) -> Result<(), Error> {
        atomic_write(node_dir, SUMMARY_FILE, &postcard::to_allocvec(self)?)
    }

    /// Read the sidecar, verifying it describes the sealed bytes named by
    /// `checksum`. Missing, stale, or unreadable all come back `None` —
    /// the caller recomputes if the node is resident and otherwise treats
    /// the summary as absent.
    pub fn read(node_dir: &Path, checksum: u64) -> Option<Self> {
        let buf = std::fs::read(node_dir.join(SUMMARY_FILE)).ok()?;
        let summary: Self = postcard::from_bytes(&buf).ok()?;
        (summary.header.version == SUMMARY_VERSION && summary.header.source_checksum == checksum)
            .then_some(summary)
    }

    /// Decode only the header — the rollup path reads thousands of these,
    /// so it must not pay for the bin arrays.
    pub fn read_header(node_dir: &Path, checksum: u64) -> Option<NodeSummaryHeader> {
        let buf = std::fs::read(node_dir.join(SUMMARY_FILE)).ok()?;
        let (header, _) = postcard::take_from_bytes::<NodeSummaryHeader>(&buf).ok()?;
        (header.version == SUMMARY_VERSION && header.source_checksum == checksum)
            .then_some(header)
    }
}

fn read_f64(prim: PrimType, bytes: &[u8]) -> f64 {
    fn arr<const N: usize>(bytes: &[u8]) -> [u8; N] {
        bytes.try_into().expect("caller sliced to prim size")
    }
    match prim {
        PrimType::U8 => bytes[0] as f64,
        PrimType::U16 => u16::from_le_bytes(arr(bytes)) as f64,
        PrimType::U32 => u32::from_le_bytes(arr(bytes)) as f64,
        PrimType::U64 => u64::from_le_bytes(arr(bytes)) as f64,
        PrimType::I8 => bytes[0] as i8 as f64,
        PrimType::I16 => i16::from_le_bytes(arr(bytes)) as f64,
        PrimType::I32 => i32::from_le_bytes(arr(bytes)) as f64,
        PrimType::I64 => i64::from_le_bytes(arr(bytes)) as f64,
        PrimType::Bool => (bytes[0] != 0) as u8 as f64,
        PrimType::F32 => f32::from_le_bytes(arr(bytes)) as f64,
        PrimType::F64 => f64::from_le_bytes(arr(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seal::SealRecordExt as _;
    use metor_proto::types::Timestamp;

    fn node_with(dir: &Path, values: &[f64]) -> (TimeSeriesNode, SealRecord) {
        let node = TimeSeriesNode::create(dir.join("0"), Timestamp(0), 8).unwrap();
        for (i, v) in values.iter().enumerate() {
            node.data.write(&v.to_le_bytes()).unwrap();
            node.index
                .write(&Timestamp(i as i64 * 10).to_le_bytes())
                .unwrap();
        }
        let seal = SealRecord::compute(&node).unwrap();
        (node, seal)
    }

    #[test]
    fn round_trips_and_validates_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let values: Vec<f64> = (0..200).map(|i| i as f64).collect();
        let (node, seal) = node_with(dir.path(), &values);
        let schema = ComponentSchema::new(PrimType::F64, &[1]);

        let summary = NodeSummary::compute(&node, &schema, &seal).unwrap();
        let node_dir = dir.path().join("0");
        summary.write(&node_dir).unwrap();

        assert_eq!(NodeSummary::read(&node_dir, seal.checksum).unwrap(), summary);
        let header = NodeSummary::read_header(&node_dir, seal.checksum).unwrap();
        assert_eq!(header, summary.header);
        assert_eq!(header.elem_min, vec![0.0]);
        assert_eq!(header.elem_max, vec![199.0]);

        // A different seal means the sidecar describes other bytes.
        assert!(NodeSummary::read(&node_dir, seal.checksum + 1).is_none());
        assert!(NodeSummary::read_header(&node_dir, seal.checksum + 1).is_none());
    }

    #[test]
    fn bins_cover_extremes_and_midpoint_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        // A narrow spike that bounded probing could miss must land in its
        // bin's max.
        let mut values: Vec<f64> = vec![1.0; 1000];
        values[500] = 100.0;
        let (node, seal) = node_with(dir.path(), &values);
        let schema = ComponentSchema::new(PrimType::F64, &[1]);

        let summary = NodeSummary::compute(&node, &schema, &seal).unwrap();
        let bins = summary.header.bins as usize;
        assert!(summary.maxs.iter().any(|&v| v == 100.0));
        assert_eq!(summary.ts.len(), bins);
        assert!(summary.ts.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn tiny_node_gets_one_bin_per_sample() {
        let dir = tempfile::tempdir().unwrap();
        let (node, seal) = node_with(dir.path(), &[3.0, 1.0, 2.0]);
        let schema = ComponentSchema::new(PrimType::F64, &[1]);
        let summary = NodeSummary::compute(&node, &schema, &seal).unwrap();
        assert_eq!(summary.header.bins, 3);
        assert_eq!(summary.mins, vec![3.0, 1.0, 2.0]);
        assert_eq!(summary.ts, vec![0, 10, 20]);
    }

    #[test]
    fn vector_elements_are_element_major() {
        let dir = tempfile::tempdir().unwrap();
        let node = TimeSeriesNode::create(dir.path().join("0"), Timestamp(0), 16).unwrap();
        for i in 0..4i64 {
            let pair = [i as f64, -(i as f64)];
            for v in pair {
                node.data.write(&v.to_le_bytes()).unwrap();
            }
            node.index
                .write(&Timestamp(i * 10).to_le_bytes())
                .unwrap();
        }
        let seal = SealRecord::compute(&node).unwrap();
        let schema = ComponentSchema::new(PrimType::F64, &[2]);

        let summary = NodeSummary::compute(&node, &schema, &seal).unwrap();
        let bins = summary.header.bins as usize;
        assert_eq!(summary.header.n_elements, 2);
        assert_eq!(bins, 4);
        assert_eq!(&summary.mins[..bins], &[0.0, 1.0, 2.0, 3.0]);
        assert_eq!(&summary.mins[bins..], &[-0.0, -1.0, -2.0, -3.0]);
        assert_eq!(summary.header.elem_max, vec![3.0, 0.0]);
    }

    #[test]
    fn non_finite_samples_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let (node, seal) = node_with(dir.path(), &[f64::NAN, 5.0, f64::INFINITY]);
        let schema = ComponentSchema::new(PrimType::F64, &[1]);
        let summary = NodeSummary::compute(&node, &schema, &seal).unwrap();
        assert!(summary.mins[0].is_nan());
        assert_eq!(summary.mins[1], 5.0);
        assert!(summary.maxs[2].is_nan());
        assert_eq!(summary.header.elem_min, vec![5.0]);
    }

    #[test]
    fn empty_node_has_no_summary() {
        let dir = tempfile::tempdir().unwrap();
        let node = TimeSeriesNode::create(dir.path().join("0"), Timestamp(0), 8).unwrap();
        let seal = SealRecord {
            start_ts: Timestamp(0),
            end_ts: Timestamp(0),
            count: 0,
            index_len: 0,
            data_len: 0,
            checksum: 0,
            element_size: 8,
        };
        let schema = ComponentSchema::new(PrimType::F64, &[1]);
        assert!(NodeSummary::compute(&node, &schema, &seal).is_none());
    }
}
