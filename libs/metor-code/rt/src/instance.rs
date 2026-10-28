//! One instance of a compiled type: its blocks and the calls over them.
//!
//! Every block is allocated once, when the instance is made. A call lends
//! the blocks to compiled code through an [`McCx`] and gathers the faults it
//! raised.

use metor_fsw_3::ParamError;
use metor_fsw_3::zerocopy::IntoBytes;
use serde_json::Value;

use crate::cx::{McCx, ResumeWord};
use crate::desc::{FrameDesc, ParamDesc, Prim, TypeDesc};
use crate::program::Program;

/// One instance's memory.
struct Blocks {
    inputs: Vec<u64>,
    present: Vec<u8>,
    outputs: Vec<u64>,
    written: Vec<u8>,
    state: Vec<u64>,
    params: Vec<u64>,
}

impl Blocks {
    /// Allocates zeroed blocks for `ty`.
    fn new(ty: &TypeDesc) -> Blocks {
        let words = |bytes: u32| vec![0u64; (bytes as usize).div_ceil(8).max(1)];
        Blocks {
            inputs: words(ty.blocks.inputs),
            present: vec![0; ty.inputs.len().max(1)],
            outputs: words(ty.blocks.outputs),
            written: vec![0; ty.outputs.len().max(1)],
            state: words(ty.blocks.state),
            params: words(ty.blocks.params),
        }
    }
}

/// An instance of one type of a [`Program`].
pub struct Instance {
    program: Program,
    ty: usize,
    blocks: Blocks,
    /// The time of the last step, which sets the next one's `dt`.
    last: Option<i64>,
    now: i64,
    dt: f64,
    /// The faults raised since the last [`Instance::take_faults`].
    faults: u32,
}

impl Instance {
    /// Makes an instance of type `ty` with its params decoded from `params`,
    /// and runs its `init`.
    pub fn new(program: Program, ty: usize, params: &Value) -> Result<Instance, ParamError> {
        let desc = program
            .desc
            .types
            .get(ty)
            .ok_or_else(|| ParamError::Decode(format!("no type {ty}")))?;
        let mut blocks = Blocks::new(desc);
        decode_params(&desc.params, params, blocks.params.as_mut_bytes())?;
        let mut instance = Instance {
            program,
            ty,
            blocks,
            last: None,
            now: 0,
            dt: 0.0,
            faults: 0,
        };
        let init = instance.desc().entry.init;
        instance.call(init);
        Ok(instance)
    }

    pub fn desc(&self) -> &TypeDesc {
        &self.program.desc.types[self.ty]
    }

    pub fn program(&self) -> &Program {
        &self.program
    }

    /// Runs `step` (a system) or `resume` (a sequence) at `now`.
    pub fn step(&mut self, now: i64) -> ResumeWord {
        self.dt = self.last.map_or(0.0, |last| (now - last) as f64 * 1e-6);
        self.last = Some(now);
        self.now = now;
        let step = self.desc().entry.step;
        ResumeWord::from_raw(self.call(step))
    }

    /// Calls redline `index` and returns whether it holds.
    pub fn redline(&mut self, index: usize, now: i64) -> bool {
        self.now = now;
        let Some(func) = self.desc().entry.redlines.get(index).map(|r| r.func) else {
            return false;
        };
        self.call(func) != 0
    }

    /// Runs the `on abort:` block, if the type has one.
    pub fn on_abort(&mut self, now: i64) {
        self.now = now;
        if let Some(func) = self.desc().entry.on_abort {
            self.call(func);
        }
    }

    /// Returns the message of the last failure.
    pub fn fail_message(&self) -> &str {
        let slot = self.desc().fail_slot as usize;
        let state = self.blocks.state.as_bytes();
        let index = state
            .get(slot..slot + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map_or(0, u32::from_ne_bytes);
        self.desc()
            .fails
            .get(index as usize)
            .map_or("failed", String::as_str)
    }

    /// Returns and clears the fault bits raised since the last call.
    pub fn take_faults(&mut self) -> u32 {
        core::mem::take(&mut self.faults)
    }

    /// Returns input `port`'s frame bytes, to copy a record into.
    pub fn input_mut(&mut self, port: usize) -> Option<&mut [u8]> {
        let (offset, size) = self.port_span(true, port)?;
        let present = self.blocks.present.get_mut(port)?;
        *present = 1;
        self.blocks.inputs.as_mut_bytes().get_mut(offset..offset + size)
    }

    /// Returns output `port`'s frame bytes.
    pub fn output(&self, port: usize) -> Option<&[u8]> {
        let (offset, size) = self.port_span(false, port)?;
        self.blocks.outputs.as_bytes().get(offset..offset + size)
    }

    /// Returns output `port`'s frame bytes, to stamp before a flush.
    pub fn output_mut(&mut self, port: usize) -> Option<&mut [u8]> {
        let (offset, size) = self.port_span(false, port)?;
        self.blocks.outputs.as_mut_bytes().get_mut(offset..offset + size)
    }

    /// Returns whether output `port` was written since it was last cleared,
    /// and clears it.
    pub fn take_written(&mut self, port: usize) -> bool {
        self.blocks
            .written
            .get_mut(port)
            .is_some_and(|flag| core::mem::take(flag) != 0)
    }

    /// Writes `now` into output `port`'s timestamp.
    pub fn stamp(&mut self, port: usize, now: i64) {
        let Some(at) = self
            .desc()
            .outputs
            .get(port)
            .and_then(|p| self.program.desc.frame_of(p))
            .map(|frame| frame.timestamp as usize)
        else {
            return;
        };
        if let Some(dst) = self.output_mut(port).and_then(|b| b.get_mut(at..at + 8)) {
            dst.copy_from_slice(&now.to_ne_bytes());
        }
    }

    /// Returns the offset and size of one port's frame in its block.
    fn port_span(&self, input: bool, port: usize) -> Option<(usize, usize)> {
        let ty = self.desc();
        let ports = if input { &ty.inputs } else { &ty.outputs };
        let port = ports.get(port)?;
        let frame = self.program.desc.frame_of(port)?;
        Some((port.offset as usize, frame.size as usize))
    }

    /// Calls compiled function `func` over the blocks.
    fn call(&mut self, func: u32) -> u32 {
        let f = self.program.func(func);
        let blocks = &mut self.blocks;
        let mut cx = McCx {
            inputs: blocks.inputs.as_ptr().cast(),
            present: blocks.present.as_ptr(),
            outputs: blocks.outputs.as_mut_ptr().cast(),
            written: blocks.written.as_mut_ptr(),
            state: blocks.state.as_mut_ptr().cast(),
            params: blocks.params.as_ptr().cast(),
            now: self.now,
            dt: self.dt,
            faults: 0,
            reserved: 0,
        };
        // SAFETY: the compiler emitted `f` for this type, and it only touches
        // the blocks at the offsets the descriptor lists, which `Blocks::new`
        // sized from that descriptor.
        let word = unsafe { f(&mut cx) };
        self.faults |= cx.faults;
        word
    }
}

/// Writes each param from `given`, or its default, into `block`.
fn decode_params(params: &[ParamDesc], given: &Value, block: &mut [u8]) -> Result<(), ParamError> {
    let given = match given {
        Value::Null => &serde_json::Map::new(),
        Value::Object(map) => map,
        other => return Err(ParamError::Decode(format!("params must be an object, not {other}"))),
    };
    if let Some(key) = given.keys().find(|k| params.iter().all(|p| &p.name != *k)) {
        return Err(ParamError::UnknownKey(key.clone()));
    }
    for param in params {
        let value = given.get(&param.name).unwrap_or(&param.default);
        let offset = param.offset as usize;
        let dst = block
            .get_mut(offset..offset + param.prim.size() as usize)
            .ok_or_else(|| ParamError::Decode(format!("`{}` lies outside its block", param.name)))?;
        write_prim(dst, param.prim, value)
            .map_err(|message| ParamError::Decode(format!("`{}`: {message}", param.name)))?;
    }
    Ok(())
}

/// Writes `value` into `dst` as a `prim`. `dst` is `prim.size()` long.
pub fn write_prim(dst: &mut [u8], prim: Prim, value: &Value) -> Result<(), String> {
    let int = || value.as_i64().ok_or_else(|| format!("{value} is not an integer"));
    let uint = || value.as_u64().ok_or_else(|| format!("{value} is not an unsigned integer"));
    let float = || value.as_f64().ok_or_else(|| format!("{value} is not a number"));
    let range = |_| format!("{value} is out of range");
    match prim {
        Prim::Bool => {
            let bit = value.as_bool().ok_or_else(|| format!("{value} is not a bool"))?;
            dst.copy_from_slice(&[u8::from(bit)]);
        }
        Prim::U8 => dst.copy_from_slice(&u8::try_from(uint()?).map_err(range)?.to_ne_bytes()),
        Prim::U16 => dst.copy_from_slice(&u16::try_from(uint()?).map_err(range)?.to_ne_bytes()),
        Prim::U32 => dst.copy_from_slice(&u32::try_from(uint()?).map_err(range)?.to_ne_bytes()),
        Prim::U64 => dst.copy_from_slice(&uint()?.to_ne_bytes()),
        Prim::I8 => dst.copy_from_slice(&i8::try_from(int()?).map_err(range)?.to_ne_bytes()),
        Prim::I16 => dst.copy_from_slice(&i16::try_from(int()?).map_err(range)?.to_ne_bytes()),
        Prim::I32 => dst.copy_from_slice(&i32::try_from(int()?).map_err(range)?.to_ne_bytes()),
        Prim::I64 => dst.copy_from_slice(&int()?.to_ne_bytes()),
        Prim::F32 => dst.copy_from_slice(&(float()? as f32).to_ne_bytes()),
        Prim::F64 => dst.copy_from_slice(&float()?.to_ne_bytes()),
    }
    Ok(())
}

/// Reads a `prim` from the start of `src` as an `f64`, or `None` if `src` is
/// too short.
pub fn read_prim(src: &[u8], prim: Prim) -> Option<f64> {
    fn take<const N: usize>(src: &[u8]) -> Option<[u8; N]> {
        src.get(..N)?.try_into().ok()
    }
    Some(match prim {
        Prim::Bool | Prim::U8 => f64::from(take::<1>(src)?[0]),
        Prim::I8 => f64::from(i8::from_ne_bytes(take(src)?)),
        Prim::U16 => f64::from(u16::from_ne_bytes(take(src)?)),
        Prim::I16 => f64::from(i16::from_ne_bytes(take(src)?)),
        Prim::U32 => f64::from(u32::from_ne_bytes(take(src)?)),
        Prim::I32 => f64::from(i32::from_ne_bytes(take(src)?)),
        Prim::U64 => u64::from_ne_bytes(take(src)?) as f64,
        Prim::I64 => i64::from_ne_bytes(take(src)?) as f64,
        Prim::F32 => f64::from(f32::from_ne_bytes(take(src)?)),
        Prim::F64 => f64::from_ne_bytes(take(src)?),
    })
}

/// Reads the leaf at `path` of a frame's bytes.
pub fn read_leaf(frame: &FrameDesc, bytes: &[u8], path: &str) -> Option<f64> {
    let leaf = frame.leaf(path)?;
    read_prim(bytes.get(leaf.offset as usize..)?, leaf.prim)
}

/// Writes `value` into the leaf at `path` of a frame's bytes.
pub fn write_leaf(frame: &FrameDesc, bytes: &mut [u8], path: &str, value: f64) -> Option<()> {
    let leaf = frame.leaf(path)?;
    let start = leaf.offset as usize;
    let dst = bytes.get_mut(start..start + leaf.prim.size() as usize)?;
    let value = match leaf.prim {
        Prim::Bool => Value::Bool(value != 0.0),
        prim if prim.is_float() => Value::from(value),
        prim if prim.is_signed() => Value::from(value as i64),
        _ => Value::from(value as u64),
    };
    write_prim(dst, leaf.prim, &value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Vec<ParamDesc> {
        vec![
            ParamDesc {
                name: "gain".into(),
                prim: Prim::F64,
                offset: 0,
                default: Value::from(2.5),
            },
            ParamDesc {
                name: "count".into(),
                prim: Prim::U8,
                offset: 8,
                default: Value::from(3),
            },
        ]
    }

    #[test]
    fn test_params_decode_with_defaults() {
        let mut block = [0u8; 16];
        let given = serde_json::json!({ "count": 7 });
        decode_params(&params(), &given, &mut block).expect("decodes");
        assert_eq!(read_prim(&block, Prim::F64), Some(2.5));
        assert_eq!(read_prim(&block[8..], Prim::U8), Some(7.0));
        decode_params(&params(), &Value::Null, &mut block).expect("defaults");
        assert_eq!(read_prim(&block[8..], Prim::U8), Some(3.0));
    }

    #[test]
    fn test_params_refuse_bad_values() {
        let mut block = [0u8; 16];
        let unknown = serde_json::json!({ "gian": 1.0 });
        assert_eq!(
            decode_params(&params(), &unknown, &mut block),
            Err(ParamError::UnknownKey("gian".into()))
        );
        let big = serde_json::json!({ "count": 300 });
        assert!(decode_params(&params(), &big, &mut block).is_err());
        let wrong = serde_json::json!({ "gain": "fast" });
        assert!(decode_params(&params(), &wrong, &mut block).is_err());
        assert!(decode_params(&params(), &Value::from(3), &mut block).is_err());
    }

    #[test]
    fn test_prims_round_trip() {
        let mut buf = [0u8; 8];
        for (prim, value) in [(Prim::I16, -3.0), (Prim::U32, 70000.0), (Prim::F32, 0.5)] {
            let json = match prim.is_float() {
                true => Value::from(value),
                false => Value::from(value as i64),
            };
            write_prim(&mut buf[..prim.size() as usize], prim, &json).expect("fits");
            assert_eq!(read_prim(&buf, prim), Some(value));
        }
        assert_eq!(read_prim(&buf[..2], Prim::F64), None);
    }
}
