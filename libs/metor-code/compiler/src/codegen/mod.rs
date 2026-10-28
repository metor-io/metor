//! Lowers a checked program to Cranelift functions in any [`Module`].
//!
//! Every entry point takes one `*mut McCx` and returns a `u32` word. Each
//! `def` takes the same context, for its faults, then its scalar params.
//! Floating point is emitted as written: no fused multiply-add and no
//! reassociation. Transcendental functions and any rounding Cranelift
//! cannot inline call `libm` inside mc-rt, never the platform's.

mod cost;
mod expr;
mod func;
mod stmt;

use std::collections::HashMap;

use cranelift_codegen::ir::{AbiParam, LibCall, Signature, types};
use cranelift_codegen::isa::CallConv;
use cranelift_module::{FuncId, Linkage, Module, ModuleError, default_libcall_names};
use mc_rt::desc::{DESC_VERSION, ProgramDesc, RedlineDesc};

use crate::hir;
use crate::types::Prim;

pub use func::Entry;

/// An error from Cranelift while lowering.
#[derive(Debug, thiserror::Error)]
pub enum CodegenError {
    #[error("cranelift: {0}")]
    Module(#[from] Box<ModuleError>),
}

impl From<ModuleError> for CodegenError {
    fn from(error: ModuleError) -> Self {
        CodegenError::Module(Box::new(error))
    }
}

/// A lowered program: its descriptor and its function table, in the order
/// the descriptor's indices name.
pub struct Lowered {
    pub desc: ProgramDesc,
    pub table: Vec<FuncId>,
}

/// The functions bodies call.
pub(crate) struct Funcs {
    pub defs: Vec<FuncId>,
    pub math: HashMap<&'static str, FuncId>,
}

/// Maps Cranelift's libcalls to mc-rt's rounding functions, and the rest to
/// their usual names.
pub fn libcall_names() -> Box<dyn Fn(LibCall) -> String + Send + Sync> {
    let default = default_libcall_names();
    Box::new(move |libcall| {
        let name = match libcall {
            LibCall::FloorF32 => "mc_rt_floorf",
            LibCall::FloorF64 => "mc_rt_floor",
            LibCall::CeilF32 => "mc_rt_ceilf",
            LibCall::CeilF64 => "mc_rt_ceil",
            LibCall::TruncF32 => "mc_rt_truncf",
            LibCall::TruncF64 => "mc_rt_trunc",
            LibCall::NearestF32 => "mc_rt_nearestf",
            LibCall::NearestF64 => "mc_rt_nearest",
            other => return default(other),
        };
        name.to_string()
    })
}

/// The Cranelift type of a scalar.
pub(crate) fn clif(prim: Prim) -> types::Type {
    match prim {
        Prim::Bool | Prim::U8 | Prim::I8 => types::I8,
        Prim::U16 | Prim::I16 => types::I16,
        Prim::U32 | Prim::I32 => types::I32,
        Prim::U64 | Prim::I64 => types::I64,
        Prim::F32 => types::F32,
        Prim::F64 => types::F64,
    }
}

/// Lowers `prog` into `module`.
pub fn lower<M: Module>(module: &mut M, prog: &hir::Program) -> Result<Lowered, CodegenError> {
    let funcs = declare_callees(module, prog)?;
    for (index, def) in prog.defs.iter().enumerate() {
        func::define_def(module, &funcs, funcs.defs[index], def)?;
    }
    let mut table = Vec::new();
    let mut types = Vec::new();
    let def_costs = cost::def_costs(&prog.defs);
    for ty in &prog.types {
        let mut desc = ty.desc.clone();
        let mut add = |module: &mut M, name: String, entry: Entry, body: &hir::Body| -> Result<u32, CodegenError> {
            let id = module.declare_function(&name, Linkage::Local, &entry_sig(module))?;
            func::define_entry(module, &funcs, id, ty, entry, body)?;
            table.push(id);
            Ok((table.len() - 1) as u32)
        };
        desc.entry.init = add(module, format!("{}_init", desc.name), Entry::Plain, &ty.init)?;
        let step = match desc.kind {
            mc_rt::desc::Kind::Cyclic => Entry::Step,
            mc_rt::desc::Kind::Sequence => Entry::Resume,
        };
        desc.entry.step = add(module, format!("{}_step", desc.name), step, &ty.step)?;
        let mut redlines = Vec::new();
        for (index, (body, named)) in ty.redlines.iter().zip(&ty.desc.entry.redlines).enumerate() {
            let func = add(module, format!("{}_redline_{index}", desc.name), Entry::Plain, body)?;
            redlines.push(RedlineDesc {
                name: named.name.clone(),
                func,
            });
        }
        desc.entry.redlines = redlines;
        if let Some(body) = &ty.on_abort {
            desc.entry.on_abort = Some(add(module, format!("{}_on_abort", desc.name), Entry::Plain, body)?);
        }
        desc.cost = cost::entry_cost(ty, &def_costs);
        types.push(desc);
    }
    let desc = ProgramDesc {
        version: DESC_VERSION,
        frames: prog.frames.iter().map(|f| f.desc()).collect(),
        types,
    };
    Ok(Lowered { desc, table })
}

/// Declares every `def`, and every math function some body calls.
fn declare_callees<M: Module>(module: &mut M, prog: &hir::Program) -> Result<Funcs, CodegenError> {
    let mut defs = Vec::new();
    for (index, def) in prog.defs.iter().enumerate() {
        let mut sig = entry_sig(module);
        sig.returns.clear();
        sig.params.extend(def.params.iter().map(|p| AbiParam::new(clif(*p))));
        sig.returns.push(AbiParam::new(clif(def.ret)));
        defs.push(module.declare_function(&format!("mc_def_{index}_{}", def.name), Linkage::Local, &sig)?);
    }
    let mut math = HashMap::new();
    for (symbol, prim, arity) in cost::math_calls(prog) {
        let mut sig = Signature::new(CallConv::triple_default(module.isa().triple()));
        sig.params.extend((0..arity).map(|_| AbiParam::new(clif(prim))));
        sig.returns.push(AbiParam::new(clif(prim)));
        let id = module.declare_function(symbol, Linkage::Import, &sig)?;
        math.insert(symbol, id);
    }
    Ok(Funcs { defs, math })
}

/// `extern "C" fn(*mut McCx) -> u32`.
pub(crate) fn entry_sig<M: Module>(module: &M) -> Signature {
    let mut sig = Signature::new(CallConv::triple_default(module.isa().triple()));
    sig.params.push(AbiParam::new(module.target_config().pointer_type()));
    sig.returns.push(AbiParam::new(types::I32));
    sig
}
