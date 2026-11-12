//! Resolves a mux's params against its bound ports into byte copies.

use std::borrow::Cow;

use metor_proto::types::PrimType;

use crate::fn_system::Bound;
use crate::frame_fields::{ElementError, FieldError, FieldTable, Leaf};
use crate::system::PortDef;

use super::cal::{Cal, F64_LEN, byte_range, f64_at, read_f64};
use super::params::{CalParam, FieldMap, MuxParams, OutParam, StampParam, TriggerParam};

/// The most inputs a mux takes; each is one bit of a `u64` mask.
pub const MAX_INPUTS: usize = 64;

/// Copies `len` bytes from offset `src` of input `input`'s record to offset
/// `dst` of an output's staging. When `cal` is set, it instead evaluates
/// `Plan::cals[cal]` on the `f64` at `src` and writes the result as an `f64`
/// at `dst`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Copy {
    pub input: u16,
    pub src: u32,
    pub dst: u32,
    pub len: u32,
    pub cal: Option<u16>,
}

/// When an output emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Input(u16),
    Any,
    All,
    Cycle,
}

/// Where an output's stamp comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stamp {
    Input(u16),
    Now,
}

/// One input: its index in `DynInputs`, its record size, and its stamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InSpec {
    pub port: usize,
    pub len: u32,
    pub stamp_at: Option<u32>,
}

/// One output: its index in `DynOutputs`, its copies, and when it emits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutSpec {
    pub port: usize,
    pub copies: Vec<Copy>,
    /// The inputs this output maps, one bit each.
    pub deps: u64,
    pub on: Trigger,
    pub stamp: Stamp,
    pub stamp_at: Option<u32>,
    /// Microseconds.
    pub max_age: Option<i64>,
    pub len: u32,
}

/// A mux's resolved inputs and outputs.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub ins: Vec<InSpec>,
    pub outs: Vec<OutSpec>,
    /// The order inputs drain in: inputs that trigger no output first.
    pub order: Vec<usize>,
    /// Calibrations, indexed by `Copy::cal`.
    pub cals: Vec<Cal>,
}

/// Why a mux's params do not fit its bound ports.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MuxError {
    #[error("a mux takes at most {MAX_INPUTS} inputs, got {0}")]
    TooManyInputs(usize),
    #[error("port `{0}` is not bound")]
    Unbound(String),
    #[error("port `{port}` carries `{record}`, which the record registry lacks")]
    UnknownRecord { port: String, record: String },
    #[error("port `{0}`'s layout differs from its record's in the registry")]
    Layout(String),
    #[error("`{0}` names an input the mux does not list")]
    UnknownInput(String),
    #[error("port `{port}`: {source}")]
    Fields { port: String, source: FieldError },
    #[error("`{port}` has no field `{path}`")]
    UnknownField { port: String, path: String },
    #[error("`{dst}` does not match `{src}` leaf for leaf")]
    Mismatch { dst: String, src: String },
    #[error("`{port}.{path}`: index past a dimension of {len}")]
    Index {
        port: String,
        path: String,
        len: usize,
    },
    #[error("`{port}.{path}` is not a scalar f64; a calibration maps f64 to f64")]
    CalType { port: String, path: String },
    #[error("`{dst}`'s calibration is empty, unsorted, or has a zero-width span")]
    CalTable { dst: String },
    #[error("a mux takes at most {MAX_CALS} calibrations, got {0}")]
    TooManyCals(usize),
    #[error("`{port}.{a}` and `{port}.{b}` overlap")]
    Overlap { port: String, a: String, b: String },
}

/// The most calibrations a mux holds; each `Copy` indexes one by `u16`.
pub const MAX_CALS: usize = u16::MAX as usize + 1;

impl Plan {
    /// Resolves `params` against the ports and records in `bound`.
    pub fn resolve(params: &MuxParams, bound: &Bound<'_>) -> Result<Self, MuxError> {
        if params.inputs.len() > MAX_INPUTS {
            return Err(MuxError::TooManyInputs(params.inputs.len()));
        }
        let ins: Vec<(usize, FieldTable)> = params
            .inputs
            .iter()
            .map(|port| port_table(&bound.def.inputs, bound, port))
            .collect::<Result<_, _>>()?;
        let mut cals = Vec::new();
        let outs: Vec<OutSpec> = params
            .outputs
            .iter()
            .map(|output| out_spec(output, &params.inputs, &ins, bound, &mut cals))
            .collect::<Result<_, _>>()?;
        let ins = ins
            .iter()
            .map(|(port, table)| InSpec {
                port: *port,
                len: table.len,
                stamp_at: table.stamp_at,
            })
            .collect();
        let order = drain_order(&outs, params.inputs.len());
        Ok(Self {
            ins,
            outs,
            order,
            cals,
        })
    }
}

fn out_spec(
    output: &OutParam,
    names: &[String],
    ins: &[(usize, FieldTable)],
    bound: &Bound<'_>,
    cals: &mut Vec<Cal>,
) -> Result<OutSpec, MuxError> {
    let (port, table) = port_table(&bound.def.outputs, bound, &output.port)?;
    let mut copies = Vec::new();
    let mut dsts = Vec::new();
    let mut deps = 0u64;
    for map in &output.fields {
        let input = input_index(names, &map.input)?;
        deps |= 1 << input;
        let mapped = map_copies(map, input, &ins[input].1, &table, &output.port, cals)?;
        dsts.extend(
            mapped
                .iter()
                .map(|c| (c.dst, c.dst.saturating_add(c.len), &*map.dst)),
        );
        copies.extend(mapped);
    }
    disjoint(dsts, &output.port)?;
    Ok(OutSpec {
        port,
        copies,
        deps,
        on: trigger(&output.on, names)?,
        stamp: stamp(&output.stamp, names)?,
        stamp_at: table.stamp_at,
        max_age: output.max_age.map(micros),
        len: table.len,
    })
}

/// The copies of `map`, one per leaf, or one calibrated copy.
fn map_copies(
    map: &FieldMap,
    input: usize,
    src: &FieldTable,
    dst: &FieldTable,
    port: &str,
    cals: &mut Vec<Cal>,
) -> Result<Vec<Copy>, MuxError> {
    match &map.cal {
        Some(cal) => Ok(vec![calibrated_copy(
            cal, map, input, src, dst, port, cals,
        )?]),
        None => field_copies(map, input, src, dst, port),
    }
}

/// Refuses two mappings writing a byte in common, given each copy's
/// `(start, end, dst path)`. An empty copy writes no byte.
fn disjoint(mut dsts: Vec<(u32, u32, &str)>, port: &str) -> Result<(), MuxError> {
    dsts.retain(|&(start, end, _)| start < end);
    dsts.sort_by_key(|&(start, ..)| start);
    match dsts.windows(2).find(|w| w[1].0 < w[0].1) {
        Some([a, b]) => Err(MuxError::Overlap {
            port: port.to_string(),
            a: a.2.to_string(),
            b: b.2.to_string(),
        }),
        _ => Ok(()),
    }
}

/// Finds `port` in `ports` and reads the registry's table for its record.
///
/// The mux declares no ports of its own before its dyn ports, so a port's
/// index in `ports` is its index in `DynInputs` or `DynOutputs`.
fn port_table(
    ports: &[PortDef],
    bound: &Bound<'_>,
    port: &str,
) -> Result<(usize, FieldTable), MuxError> {
    let index = ports
        .iter()
        .position(|def| def.name == port)
        .ok_or_else(|| MuxError::Unbound(port.to_string()))?;
    let def = &ports[index];
    let registry = bound
        .records
        .get(&def.record)
        .ok_or_else(|| MuxError::UnknownRecord {
            port: port.to_string(),
            record: def.record.to_string(),
        })?;
    if def.layout != registry.layout {
        return Err(MuxError::Layout(port.to_string()));
    }
    let table = FieldTable::of(registry).map_err(|source| MuxError::Fields {
        port: port.to_string(),
        source,
    })?;
    Ok((index, table))
}

fn input_index(names: &[String], port: &str) -> Result<usize, MuxError> {
    names
        .iter()
        .position(|name| name == port)
        .ok_or_else(|| MuxError::UnknownInput(port.to_string()))
}

fn trigger(on: &TriggerParam, names: &[String]) -> Result<Trigger, MuxError> {
    Ok(match on {
        TriggerParam::Input(port) => Trigger::Input(to_u16(input_index(names, port)?)),
        TriggerParam::Any => Trigger::Any,
        TriggerParam::All => Trigger::All,
        TriggerParam::Cycle => Trigger::Cycle,
    })
}

fn stamp(stamp: &StampParam, names: &[String]) -> Result<Stamp, MuxError> {
    Ok(match stamp {
        StampParam::Input(port) => Stamp::Input(to_u16(input_index(names, port)?)),
        StampParam::Now => Stamp::Now,
    })
}

/// Every input, those that trigger no output first, each group by index.
fn drain_order(outs: &[OutSpec], count: usize) -> Vec<usize> {
    let triggers = |i: usize| {
        outs.iter()
            .any(|out| matches!(out.on, Trigger::Input(t) if usize::from(t) == i))
    };
    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by_key(|&i| triggers(i));
    order
}

const _: () = assert!(MAX_INPUTS <= u16::MAX as usize);

/// An input index, which `resolve` keeps below `MAX_INPUTS`.
fn to_u16(index: usize) -> u16 {
    u16::try_from(index).unwrap_or(u16::MAX)
}

fn micros(duration: core::time::Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

/// A leaf of a mapping, by its path relative to the mapped path.
type Rel<'a> = (&'a str, Cow<'a, Leaf>);

/// The leaves under `path`, by their path relative to it, sorted; or the
/// element at `path` alone, at relative path `""`.
fn relative<'a>(table: &'a FieldTable, path: &'a str) -> Result<Vec<Rel<'a>>, ElementError> {
    let mut leaves: Vec<Rel<'a>> = table
        .under(path)
        .map(|leaf| (&leaf.path[path.len()..], Cow::Borrowed(leaf)))
        .collect();
    if leaves.is_empty() {
        return Ok(vec![("", Cow::Owned(table.element(path)?))]);
    }
    leaves.sort_unstable_by_key(|(rel, _)| *rel);
    Ok(leaves)
}

/// `relative`, with its error named for `port`.
fn side<'a>(table: &'a FieldTable, path: &'a str, port: &str) -> Result<Vec<Rel<'a>>, MuxError> {
    relative(table, path).map_err(|e| named(e, port, path))
}

fn named(e: ElementError, port: &str, path: &str) -> MuxError {
    match e {
        ElementError::Unknown => MuxError::UnknownField {
            port: port.to_string(),
            path: path.to_string(),
        },
        ElementError::Index { len } => MuxError::Index {
            port: port.to_string(),
            path: path.to_string(),
            len,
        },
    }
}

/// One copy per leaf of `map`, pairing leaves by relative path.
fn field_copies(
    map: &FieldMap,
    input: usize,
    src: &FieldTable,
    dst: &FieldTable,
    port: &str,
) -> Result<Vec<Copy>, MuxError> {
    let dsts = side(dst, &map.dst, port)?;
    let srcs = side(src, &map.src, &map.input)?;
    let same =
        |(d, s): (&Rel<'_>, &Rel<'_>)| d.0 == s.0 && d.1.ty == s.1.ty && d.1.shape == s.1.shape;
    if dsts.len() != srcs.len() || !dsts.iter().zip(&srcs).all(same) {
        return Err(MuxError::Mismatch {
            dst: format!("{port}.{}", map.dst),
            src: format!("{}.{}", map.input, map.src),
        });
    }
    let copy = |((_, d), (_, s)): (&Rel<'_>, &Rel<'_>)| Copy {
        input: to_u16(input),
        src: s.offset,
        dst: d.offset,
        len: d.len,
        cal: None,
    };
    Ok(dsts.iter().zip(&srcs).map(copy).collect())
}

/// One copy from a scalar `f64` through `param` to a scalar `f64`, its
/// calibration pushed onto `cals`.
fn calibrated_copy(
    param: &CalParam,
    map: &FieldMap,
    input: usize,
    src: &FieldTable,
    dst: &FieldTable,
    port: &str,
    cals: &mut Vec<Cal>,
) -> Result<Copy, MuxError> {
    let dst_at = scalar_f64(dst, &map.dst, port)?;
    let src_at = scalar_f64(src, &map.src, &map.input)?;
    let cal = lower(param, map, src, port)?;
    Ok(Copy {
        input: to_u16(input),
        src: src_at,
        dst: dst_at,
        len: F64_LEN,
        cal: Some(push_cal(cals, cal)?),
    })
}

/// The offset of the scalar `f64` leaf or element at `path`.
fn scalar_f64(table: &FieldTable, path: &str, port: &str) -> Result<u32, MuxError> {
    match side(table, path, port)?.as_slice() {
        [(rel, leaf)] if rel.is_empty() && leaf.ty == PrimType::F64 && leaf.shape.is_empty() => {
            Ok(leaf.offset)
        }
        _ => Err(MuxError::CalType {
            port: port.to_string(),
            path: path.to_string(),
        }),
    }
}

/// Builds `param` for `map`, resolving an excitation in the mapping's input.
fn lower(param: &CalParam, map: &FieldMap, src: &FieldTable, port: &str) -> Result<Cal, MuxError> {
    let bad = || MuxError::CalTable {
        dst: format!("{port}.{}", map.dst),
    };
    match param {
        CalParam::Poly(coeffs) => Cal::poly(coeffs).ok_or_else(bad),
        CalParam::Table(points) => Cal::table(points).ok_or_else(bad),
        CalParam::Thermocouple { kind } => Ok(Cal::Thermocouple { kind: *kind }),
        CalParam::Ratiometric {
            excitation,
            span,
            eng,
        } => {
            let at = scalar_f64(src, excitation, &map.input)?;
            Cal::ratiometric(at, *span, *eng).ok_or_else(bad)
        }
    }
}

/// Pushes `cal` and returns its index.
fn push_cal(cals: &mut Vec<Cal>, cal: Cal) -> Result<u16, MuxError> {
    let index = u16::try_from(cals.len()).map_err(|_| MuxError::TooManyCals(cals.len() + 1))?;
    cals.push(cal);
    Ok(index)
}

/// Applies `c` from `src` into `dst`; `false` when out of range or `c`'s
/// calibration is not in `cals`.
pub fn apply(c: &Copy, cals: &[Cal], src: &[u8], dst: &mut [u8]) -> bool {
    match c.cal {
        None => copy_bytes(c, src, dst),
        Some(i) => cals
            .get(usize::from(i))
            .is_some_and(|cal| calibrate(cal, c, src, dst)),
    }
}

fn copy_bytes(c: &Copy, src: &[u8], dst: &mut [u8]) -> bool {
    let (Some(from), Some(to)) = (byte_range(c.src, c.len), byte_range(c.dst, c.len)) else {
        return false;
    };
    match (src.get(from), dst.get_mut(to)) {
        (Some(from), Some(to)) => {
            to.copy_from_slice(from);
            true
        }
        _ => false,
    }
}

/// Writes `cal` of the `f64` at `c.src` as an `f64` at `c.dst`.
fn calibrate(cal: &Cal, c: &Copy, src: &[u8], dst: &mut [u8]) -> bool {
    let Some(x) = read_f64(src, c.src) else {
        return false;
    };
    let Some(to) = f64_at(c.dst).and_then(|range| dst.get_mut(range)) else {
        return false;
    };
    to.copy_from_slice(&cal.eval(x, src).to_le_bytes());
    true
}

#[cfg(all(test, feature = "host"))]
pub(crate) mod tests;
