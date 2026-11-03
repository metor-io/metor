//! Answering [`GetTimeSeries`]: which samples a reply carries, and the range
//! it vouches for.
//!
//! A reply's `covered` range is what lets a client cache it: inside it, the
//! client holds every sample this server holds, so "checked and empty" is
//! distinguishable from "not checked". It is therefore computed
//! conservatively. It stops at the first stretch this server doesn't hold
//! itself (a span it only knows remotely, or tiered out), at the limit and
//! at the size budget, and never splits a run of equal timestamps: a reply
//! either carries every sample at a timestamp or vouches for none of them.

use std::ops::Range;

use metor_proto::types::{LenPacket, PacketHeader, Timestamp};
use metor_proto_wkt::{GetTimeSeries, SeriesEnd};
use zerocopy::IntoBytes;

use crate::{
    Error,
    manifest::GapVec,
    time_series::{TimeSeries, TimeSeriesNodeSlice},
};

/// Upper bound on a `GetTimeSeries` response, header included.
pub(crate) const SERIES_BUDGET: usize = 256 * 1024 * 1024;
/// A time series packet's bytes before its timestamps: header, count and
/// covered range.
const SERIES_HEAD: usize = size_of::<PacketHeader>() + metor_proto::types::TimeSeries::HEAD;

/// Samples of `size` bytes one reply has room for.
pub(crate) fn fit(size: usize) -> Result<usize, Error> {
    let entry = size.saturating_add(size_of::<Timestamp>());
    match (SERIES_BUDGET - SERIES_HEAD) / entry {
        0 => Err(Error::ComponentTooLarge(size)),
        fit => Ok(fit),
    }
}

/// The samples of one query range as a single array, oldest first, over
/// the per-node slices a [`TimeSeries`] hands out.
pub(crate) struct Flat {
    slices: Vec<TimeSeriesNodeSlice>,
    /// Cumulative sample counts: slice `i` holds `ends[i - 1]..ends[i]`.
    ends: Vec<usize>,
}

impl Flat {
    /// The samples of `series` within `range`, end exclusive.
    pub(crate) fn new(series: &TimeSeries, range: &Range<Timestamp>) -> Self {
        let mut flat = Self {
            slices: Vec::new(),
            ends: Vec::new(),
        };
        if range.end.0 <= range.start.0 {
            return flat;
        }
        let Some(slice) = series.get_range(range.start..Timestamp(range.end.0 - 1)) else {
            return flat;
        };
        let mut slices: Vec<_> = slice.as_iter().collect();
        slices.reverse();
        let mut total = 0;
        for slice in slices {
            let len = slice.timestamps().len();
            if len == 0 {
                continue;
            }
            total += len;
            flat.ends.push(total);
            flat.slices.push(slice);
        }
        flat
    }

    pub(crate) fn len(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }

    fn locate(&self, i: usize) -> (&TimeSeriesNodeSlice, usize) {
        let s = self.ends.partition_point(|end| *end <= i);
        let start = s.checked_sub(1).map_or(0, |prev| self.ends[prev]);
        (&self.slices[s], i - start)
    }

    pub(crate) fn stamp(&self, i: usize) -> Timestamp {
        let (slice, at) = self.locate(i);
        slice.timestamps()[at]
    }

    /// The first index whose timestamp fails `pred`, which must hold for a
    /// prefix of the array.
    fn partition_point(&self, pred: impl Fn(Timestamp) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(self.stamp(mid)) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// The indices `lo..hi` of the samples stamped `t`.
    fn tie(&self, t: Timestamp) -> Range<usize> {
        self.partition_point(|s| s.0 < t.0)..self.partition_point(|s| s.0 <= t.0)
    }

    /// Copy the samples `picks` selects into `stamps` and `data`, which are
    /// sized for them.
    fn copy(
        &self,
        picks: &Picks,
        size: usize,
        stamps: &mut [u8],
        data: &mut [u8],
    ) -> Result<(), Error> {
        const STAMP: usize = size_of::<Timestamp>();
        match picks {
            Picks::Run(run) => {
                let mut out = 0;
                for (s, slice) in self.slices.iter().enumerate() {
                    let start = s.checked_sub(1).map_or(0, |prev| self.ends[prev]);
                    let (lo, hi) = (run.start.max(start), run.end.min(self.ends[s]));
                    if lo >= hi {
                        continue;
                    }
                    let (lo, hi, n) = (lo - start, hi - start, hi - lo);
                    stamps[out * STAMP..(out + n) * STAMP]
                        .copy_from_slice(slice.timestamps()[lo..hi].as_bytes());
                    let src = slice
                        .data()
                        .get(lo * size..hi * size)
                        .ok_or(Error::SeriesTruncated)?;
                    data[out * size..(out + n) * size].copy_from_slice(src);
                    out += n;
                }
                match out == run.len() {
                    true => Ok(()),
                    false => Err(Error::SeriesTruncated),
                }
            }
            Picks::Cells(cells) => {
                for (out, i) in cells.iter().enumerate() {
                    let (slice, at) = self.locate(*i);
                    stamps[out * STAMP..(out + 1) * STAMP]
                        .copy_from_slice(slice.timestamps()[at].as_bytes());
                    let src = slice
                        .data()
                        .get(at * size..(at + 1) * size)
                        .ok_or(Error::SeriesTruncated)?;
                    data[out * size..(out + 1) * size].copy_from_slice(src);
                }
                Ok(())
            }
        }
    }
}

/// Which samples of a [`Flat`] a reply carries, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Picks {
    Run(Range<usize>),
    /// One sample per stride cell.
    Cells(Vec<usize>),
}

impl Picks {
    fn len(&self) -> usize {
        match self {
            Picks::Run(run) => run.len(),
            Picks::Cells(cells) => cells.len(),
        }
    }
}

/// A planned reply: the samples to send and what they vouch for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    pub picks: Picks,
    pub covered: Range<Timestamp>,
}

/// The part of `range` this server answers for from data it holds itself:
/// from the requested end up to the first stretch it only knows remotely.
pub(crate) fn held(
    series: &TimeSeries,
    range: &Range<Timestamp>,
    from: SeriesEnd,
) -> Range<Timestamp> {
    if range.end.0 <= range.start.0 {
        return range.start..range.start;
    }
    let mut gaps = GapVec::new();
    series.manifest().gaps(range, &mut gaps);
    match from {
        SeriesEnd::Oldest => {
            let end = gaps
                .iter()
                .map(|g| g.range.start)
                .min()
                .unwrap_or(range.end);
            range.start..end
        }
        SeriesEnd::Newest => {
            let start = gaps
                .iter()
                .map(|g| g.range.end)
                .max()
                .unwrap_or(range.start);
            start..range.end
        }
    }
}

/// Plan the reply to `req` over `flat`, the samples of `held`.
pub(crate) fn plan(
    flat: &Flat,
    held: Range<Timestamp>,
    req: &GetTimeSeries,
    size: usize,
) -> Result<Reply, Error> {
    let fit = fit(size)?;
    let limit = req.limit.unwrap_or(usize::MAX);
    match req.stride {
        Some(stride) => Ok(cells(
            flat,
            held,
            &req.range,
            req.from,
            limit.min(fit),
            stride,
        )),
        None => run(flat, held, req.from, limit.min(fit), fit, size),
    }
}

/// The `len` samples at `from`'s end of `flat`, moved so no timestamp is
/// split: back to the last timestamp change, or, if the page is one run of
/// equal timestamps, out to hold all of it (up to `cap`, past any limit).
fn run(
    flat: &Flat,
    held: Range<Timestamp>,
    from: SeriesEnd,
    len: usize,
    cap: usize,
    size: usize,
) -> Result<Reply, Error> {
    let n = flat.len();
    let len = len.min(n);
    let whole = |run: Range<usize>| -> Result<Range<usize>, Error> {
        match run.len() <= cap {
            true => Ok(run),
            false => Err(Error::ComponentTooLarge(size)),
        }
    };
    match from {
        SeriesEnd::Oldest => {
            if len == n {
                return Ok(Reply {
                    picks: Picks::Run(0..n),
                    covered: held,
                });
            }
            if len == 0 {
                return Ok(Reply {
                    picks: Picks::Run(0..0),
                    covered: held.start..held.start,
                });
            }
            let mut end = len;
            let edge = flat.stamp(end - 1);
            if flat.stamp(end) == edge {
                let tie = flat.tie(edge);
                end = match tie.start {
                    0 => whole(0..tie.end)?.end,
                    start => start,
                };
            }
            let covered = match end == n {
                true => held,
                false => held.start..flat.stamp(end),
            };
            Ok(Reply {
                picks: Picks::Run(0..end),
                covered,
            })
        }
        SeriesEnd::Newest => {
            if len == n {
                return Ok(Reply {
                    picks: Picks::Run(0..n),
                    covered: held,
                });
            }
            if len == 0 {
                return Ok(Reply {
                    picks: Picks::Run(n..n),
                    covered: held.end..held.end,
                });
            }
            let mut start = n - len;
            let edge = flat.stamp(start);
            if flat.stamp(start - 1) == edge {
                let tie = flat.tie(edge);
                start = match tie.end {
                    end if end == n => whole(tie.start..n)?.start,
                    end => end,
                };
            }
            let covered = match start {
                0 => held,
                start => Timestamp(flat.stamp(start - 1).0 + 1)..held.end,
            };
            Ok(Reply {
                picks: Picks::Run(start..n),
                covered,
            })
        }
    }
}

/// The last sample of each stride cell, at most `limit` cells counted from
/// `from`'s end. Each cell costs one binary search; empty cells cost none.
///
/// `covered` ends on cell boundaries wherever this server chose the edge:
/// at a limit, and where `held` stops short of the requested `range` at a
/// stretch it only knows remotely, since a cell cut there might hold a
/// later sample than the one it would report. A cell cut by that is left
/// out, pick and all. An edge the request itself set is kept as asked, so
/// a cell it cuts reports its last sample within `range`; requests whose
/// replies are merged align their ranges to the stride.
fn cells(
    flat: &Flat,
    held: Range<Timestamp>,
    range: &Range<Timestamp>,
    from: SeriesEnd,
    limit: usize,
    stride: std::time::Duration,
) -> Reply {
    let width = i64::try_from(stride.as_micros()).unwrap_or(i64::MAX).max(1);
    let cell = |t: Timestamp| t.0.div_euclid(width);
    let cell_start = |c: i64| Timestamp(c.saturating_mul(width));
    let n = flat.len();
    let mut picks = Vec::new();
    let mut covered = match from {
        SeriesEnd::Oldest => {
            let mut i = 0;
            while i < n && picks.len() < limit {
                let next = cell_start(cell(flat.stamp(i)).saturating_add(1));
                // The next cell's start saturates at the end of time; always
                // step past the cell, so a sample there (unreachable today,
                // ranges being end-exclusive) could never stall the walk.
                let end = flat.partition_point(|t| t.0 < next.0).max(i + 1);
                picks.push(end - 1);
                i = end;
            }
            match i < n {
                true => held.start..cell_start(cell(flat.stamp(i))).max(held.start),
                false => held,
            }
        }
        SeriesEnd::Newest => {
            let mut end = n;
            while end > 0 && picks.len() < limit {
                let start = cell_start(cell(flat.stamp(end - 1)));
                picks.push(end - 1);
                end = flat.partition_point(|t| t.0 < start.0);
            }
            picks.reverse();
            match end > 0 {
                true => {
                    cell_start(cell(flat.stamp(end - 1)).saturating_add(1)).min(held.end)..held.end
                }
                false => held,
            }
        }
    };
    let aligned = |t: Timestamp| cell_start(cell(t)) == t;
    if covered.end != range.end && !aligned(covered.end) {
        covered.end = cell_start(cell(covered.end)).max(covered.start);
    }
    if covered.start != range.start && !aligned(covered.start) {
        covered.start = cell_start(cell(covered.start).saturating_add(1)).min(covered.end);
    }
    picks.retain(|i| covered.contains(&flat.stamp(*i)));
    Reply {
        picks: Picks::Cells(picks),
        covered,
    }
}

/// Fill `pkt`, a cleared time series packet whose header is set, with
/// `reply`'s samples of `flat`.
pub(crate) fn write(
    pkt: &mut LenPacket,
    flat: &Flat,
    reply: &Reply,
    size: usize,
) -> Result<(), Error> {
    let len = reply.picks.len();
    let stamps_len = len * size_of::<Timestamp>();
    let body_len = stamps_len + len * size;
    let pkt_len =
        u32::try_from(SERIES_HEAD + body_len).map_err(|_| Error::ComponentTooLarge(size))?;
    pkt.extend_from_slice((len as u64).as_bytes());
    pkt.extend_from_slice(reply.covered.start.0.as_bytes());
    pkt.extend_from_slice(reply.covered.end.0.as_bytes());
    pkt.inner.resize(pkt.inner.len() + body_len, 0);
    pkt.inner[..size_of::<u32>()].copy_from_slice(&pkt_len.to_le_bytes());
    let body = &mut pkt.as_mut_packet().body[metor_proto::types::TimeSeries::HEAD..];
    let (stamps, data) = body.split_at_mut(stamps_len);
    flat.copy(&reply.picks, size, stamps, data)
}

/// Plan and write the reply to `req` against `series` into `pkt`.
pub(crate) fn answer(
    pkt: &mut LenPacket,
    series: &TimeSeries,
    req: &GetTimeSeries,
    size: usize,
) -> Result<(), Error> {
    let held = held(series, &req.range, req.from);
    let flat = Flat::new(series, &held);
    let reply = plan(&flat, held, req, size)?;
    write(pkt, &flat, &reply, size)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use metor_proto::types::ComponentId;

    use super::*;
    use crate::seal::SealRecord;

    fn series(dir: &std::path::Path, stamps: &[i64]) -> TimeSeries {
        let series = TimeSeries::create(dir).unwrap();
        let mut writer = series.writer().unwrap();
        for (i, ts) in stamps.iter().enumerate() {
            writer
                .push_buf(Timestamp(*ts), &(i as u64).to_le_bytes())
                .unwrap();
        }
        series
    }

    fn ask(series: &TimeSeries, req: &GetTimeSeries) -> (Vec<i64>, Range<i64>) {
        let held = held(series, &req.range, req.from);
        let flat = Flat::new(series, &held);
        let reply = plan(&flat, held, req, 8).unwrap();
        let picks: Vec<usize> = match &reply.picks {
            Picks::Run(run) => run.clone().collect(),
            Picks::Cells(cells) => cells.clone(),
        };
        (
            picks.iter().map(|i| flat.stamp(*i).0).collect(),
            reply.covered.start.0..reply.covered.end.0,
        )
    }

    fn req(start: i64, end: i64) -> GetTimeSeries {
        GetTimeSeries::range(ComponentId::new("a"), Timestamp(start)..Timestamp(end))
    }

    #[test]
    fn oldest_pages_cover_up_to_the_next_sample() {
        let dir = tempfile::tempdir().unwrap();
        let s = series(dir.path(), &[10, 20, 30, 40]);
        assert_eq!(ask(&s, &req(0, 100)), (vec![10, 20, 30, 40], 0..100));
        assert_eq!(ask(&s, &req(0, 100).limit(2)), (vec![10, 20], 0..30));
        // The range end is exclusive.
        assert_eq!(ask(&s, &req(0, 40)), (vec![10, 20, 30], 0..40));
        assert_eq!(ask(&s, &req(0, 100).limit(0)), (vec![], 0..0));
    }

    #[test]
    fn newest_walks_back_from_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let s = series(dir.path(), &[10, 20, 30, 40]);
        assert_eq!(
            ask(&s, &req(0, 100).newest().limit(2)),
            (vec![30, 40], 21..100)
        );
        // An instant: the newest sample at or before it, vouching back to
        // just past the sample before that.
        let at = |t| GetTimeSeries::instant(ComponentId::new("a"), Timestamp(t));
        assert_eq!(ask(&s, &at(35)), (vec![30], 21..36));
        assert_eq!(ask(&s, &at(30)), (vec![30], 21..31));
        assert_eq!(ask(&s, &at(10)), (vec![10], i64::MIN..11));
        assert_eq!(ask(&s, &at(5)), (vec![], i64::MIN..6));
    }

    #[test]
    fn limits_never_split_a_tie() {
        let dir = tempfile::tempdir().unwrap();
        let s = series(dir.path(), &[1, 2, 3, 3, 3, 4, 5]);
        assert_eq!(ask(&s, &req(0, 10).limit(3)), (vec![1, 2], 0..3));
        assert_eq!(ask(&s, &req(3, 10).limit(1)), (vec![3, 3, 3], 3..4));
        assert_eq!(ask(&s, &req(0, 10).newest().limit(4)), (vec![4, 5], 4..10));
        assert_eq!(ask(&s, &req(0, 4).newest().limit(1)), (vec![3, 3, 3], 3..4));
        let at = GetTimeSeries::instant(ComponentId::new("a"), Timestamp(3));
        assert_eq!(ask(&s, &at), (vec![3, 3, 3], 3..4));
    }

    #[test]
    fn strides_keep_the_last_sample_per_cell() {
        let dir = tempfile::tempdir().unwrap();
        let s = series(dir.path(), &[1, 4, 9, 10, 11, 25, 29, 41]);
        let ten = Duration::from_micros(10);
        assert_eq!(
            ask(&s, &req(0, 50).stride(ten)),
            (vec![9, 11, 29, 41], 0..50)
        );
        assert_eq!(
            ask(&s, &req(0, 50).stride(ten).limit(2)),
            (vec![9, 11], 0..20)
        );
        assert_eq!(
            ask(&s, &req(0, 50).stride(ten).newest().limit(2)),
            (vec![29, 41], 20..50)
        );
        // Cells are epoch-aligned, not aligned to the range.
        assert_eq!(ask(&s, &req(5, 26).stride(ten)), (vec![9, 11, 25], 5..26));
    }

    #[test]
    fn an_empty_series_vouches_for_the_whole_range() {
        let dir = tempfile::tempdir().unwrap();
        let s = TimeSeries::create(dir.path()).unwrap();
        assert_eq!(ask(&s, &req(0, 100)), (vec![], 0..100));
        assert_eq!(ask(&s, &req(0, 100).newest().limit(1)), (vec![], 0..100));
    }

    #[test]
    fn coverage_stops_at_what_is_only_held_remotely() {
        let dir = tempfile::tempdir().unwrap();
        let s = series(dir.path(), &[10, 20, 30]);
        let remote = SealRecord {
            start_ts: Timestamp(100),
            end_ts: Timestamp(199),
            count: 10,
            index_len: 80,
            data_len: 80,
            checksum: 7,
            element_size: 8,
        };
        s.merge_remote_spans([remote]).unwrap();
        assert_eq!(ask(&s, &req(0, 400)), (vec![10, 20, 30], 0..100));
        assert_eq!(ask(&s, &req(0, 400).newest()), (vec![], 200..400));
        let at = |t| GetTimeSeries::instant(ComponentId::new("a"), Timestamp(t));
        assert_eq!(ask(&s, &at(150)), (vec![], 151..151));
        assert_eq!(ask(&s, &at(250)), (vec![], 200..251));
        assert_eq!(ask(&s, &at(50)), (vec![30], 21..51));
    }

    /// Where the server stops at a stretch it only knows remotely, the cell
    /// that cuts is left out: its last sample may lie beyond.
    #[test]
    fn strided_coverage_stops_on_a_cell_boundary_at_a_remote_stretch() {
        let dir = tempfile::tempdir().unwrap();
        let s = series(dir.path(), &[1, 4, 9, 11, 14]);
        let remote = SealRecord {
            start_ts: Timestamp(15),
            end_ts: Timestamp(40),
            count: 3,
            index_len: 24,
            data_len: 24,
            checksum: 7,
            element_size: 8,
        };
        s.merge_remote_spans([remote]).unwrap();
        let ten = Duration::from_micros(10);
        assert_eq!(ask(&s, &req(0, 30).stride(ten)), (vec![9], 0..10));
        assert_eq!(
            ask(&s, &req(41, 100).stride(ten).newest()),
            (vec![], 41..100)
        );
    }

    #[test]
    fn strides_reach_the_end_of_time() {
        let dir = tempfile::tempdir().unwrap();
        let s = series(dir.path(), &[5, i64::MAX - 1]);
        let req = req(0, i64::MAX).stride(Duration::from_micros(u64::MAX));
        assert_eq!(ask(&s, &req), (vec![i64::MAX - 1], 0..i64::MAX));
    }

    #[test]
    fn a_tie_past_the_cap_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let s = series(dir.path(), &[1, 1, 1, 2]);
        let held = held(&s, &(Timestamp(0)..Timestamp(10)), SeriesEnd::Oldest);
        let flat = Flat::new(&s, &held);
        assert!(run(&flat, held.clone(), SeriesEnd::Oldest, 1, 3, 8).is_ok());
        assert!(matches!(
            run(&flat, held.clone(), SeriesEnd::Oldest, 1, 2, 8),
            Err(Error::ComponentTooLarge(8))
        ));
        assert_eq!(
            run(&flat, held, SeriesEnd::Oldest, 4, 2, 8).unwrap().picks,
            Picks::Run(0..4)
        );
    }
}
