//! Compiles a file into memory with `cranelift-jit`, for tests and the dev
//! loop. The result steps through the same mc-rt as a flight pack.

use std::rc::Rc;

use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::FuncId;
use mc_rt::{EntryFn, Program};

use crate::CompileError;
use crate::check::Imports;
use crate::codegen::{self, CodegenError};

/// Keeps a JIT's code mapped for as long as a program uses it.
struct Owner(#[allow(dead_code)] JITModule);

/// Compiles `src` and returns a program mc-rt can step in this process.
pub fn compile(src: &str, imports: &dyn Imports) -> Result<Program, CompileError> {
    let prog = crate::front(src, imports)?;
    let mut builder = JITBuilder::with_flags(&[("opt_level", "speed")], codegen::libcall_names())
        .map_err(CodegenError::from)?;
    for math in mc_rt::math::MATH.iter().chain(mc_rt::math::ROUNDING) {
        builder.symbol(math.symbol, math.addr);
    }
    let mut module = JITModule::new(builder);
    let lowered = codegen::lower(&mut module, &prog)?;
    module.finalize_definitions().map_err(CodegenError::from)?;
    let fns = lowered.table.iter().map(|id| entry(&module, *id)).collect();
    let owner: Rc<dyn std::any::Any> = Rc::new(Owner(module));
    Ok(Program::new(lowered.desc, fns, Some(owner))?)
}

/// The address of a finalized entry point, as an entry function.
fn entry(module: &JITModule, id: FuncId) -> EntryFn {
    let code = module.get_finalized_function(id);
    // SAFETY: every function in the table was declared with the entry
    // signature, `extern "C" fn(*mut McCx) -> u32`, and is finalized.
    unsafe { core::mem::transmute::<*const u8, EntryFn>(code) }
}
