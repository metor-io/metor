//! One function being lowered: its builder, its context pointer, and the
//! blocks a sequence's resume points jump to.

use std::collections::HashMap;

use cranelift_codegen::ir::{
    AbiParam, Block, FuncRef, InstBuilder, JumpTableData, MemFlagsData, Type, Value, types,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Module};
use mc_rt::cx::offsets;

use super::{CodegenError, Funcs, clif, entry_sig};
use crate::hir::{self, Base};
use crate::types::Prim;

/// What kind of entry point a body is lowered as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// Runs its statements; a `return` gives the word.
    Plain,
    /// A system's step: alerts, then the body.
    Step,
    /// A sequence's resume: alerts, then a jump to the resume point.
    Resume,
}

/// The state a sequence's resume reads and writes.
pub(crate) struct SeqCx {
    pub resume_slot: u32,
    pub fail_slot: u32,
    /// The block each resume point continues at, by point; 0 is the start.
    pub points: Vec<Block>,
}

pub(crate) struct FnCx<'f, 'm, M: Module> {
    pub b: FunctionBuilder<'f>,
    pub module: &'m mut M,
    pub funcs: &'m Funcs,
    refs: HashMap<FuncId, FuncRef>,
    pub ptr: Type,
    pub cx: Value,
    /// The block pointers, by [`Base`], and the written and present masks.
    pub inputs: Value,
    pub present: Value,
    pub outputs: Value,
    pub written: Value,
    pub state: Value,
    pub params: Value,
    pub locals: Vec<Variable>,
    /// The fault bits raised so far.
    pub faults: Variable,
    pub seq: Option<SeqCx>,
    /// The type a `def` returns, or `None` for an entry point.
    pub ret: Option<Prim>,
}

pub(crate) const FLAGS: MemFlagsData = MemFlagsData::trusted();

impl<M: Module> FnCx<'_, '_, M> {
    pub fn func_ref(&mut self, id: FuncId) -> FuncRef {
        if let Some(r) = self.refs.get(&id) {
            return *r;
        }
        let r = self.module.declare_func_in_func(id, self.b.func);
        self.refs.insert(id, r);
        r
    }

    pub fn base(&self, base: Base) -> Value {
        match base {
            Base::Input => self.inputs,
            Base::Output => self.outputs,
            Base::State => self.state,
            Base::Params => self.params,
        }
    }

    /// Ends the current block, if it is still open, and opens a new one
    /// that nothing jumps to, so statements after a `return` still have a
    /// block.
    pub fn after_return(&mut self) {
        let dead = self.b.create_block();
        self.b.switch_to_block(dead);
    }

    /// Writes the faults into the context and returns `word`.
    pub fn ret_word(&mut self, word: Value) {
        self.flush_faults();
        self.b.ins().return_(&[word]);
        self.after_return();
    }

    pub fn ret_const(&mut self, word: u32) {
        let word = self.b.ins().iconst(types::I32, i64::from(word));
        self.ret_word(word);
    }

    /// Ors this function's faults into `cx.faults`.
    pub fn flush_faults(&mut self) {
        let mine = self.b.use_var(self.faults);
        let theirs = self
            .b
            .ins()
            .load(types::I32, FLAGS, self.cx, offsets::FAULTS);
        let both = self.b.ins().bor(mine, theirs);
        self.b.ins().store(FLAGS, both, self.cx, offsets::FAULTS);
    }

    /// Raises `fault` when `cond` (an `i8`) is set.
    pub fn fault_if(&mut self, cond: Value, fault: mc_rt::Fault) {
        let bits = self.b.ins().uextend(types::I32, cond);
        let shift = (fault as u32).trailing_zeros();
        let bits = self.b.ins().ishl_imm_s(bits, i64::from(shift));
        let old = self.b.use_var(self.faults);
        let new = self.b.ins().bor(old, bits);
        self.b.def_var(self.faults, new);
    }

    /// Returns zero of `prim`.
    pub fn zero(&mut self, prim: Prim) -> Value {
        match prim {
            Prim::F32 => self.b.ins().f32const(0.0),
            Prim::F64 => self.b.ins().f64const(0.0),
            p => self.b.ins().iconst(clif(p), 0),
        }
    }
}

/// Builds a function's entry block: the context pointer, the block
/// pointers, the locals, and the fault accumulator.
fn open<'f, 'm, M: Module>(
    module: &'m mut M,
    funcs: &'m Funcs,
    mut b: FunctionBuilder<'f>,
    locals: &[Prim],
    params: usize,
    ret: Option<Prim>,
) -> FnCx<'f, 'm, M> {
    let ptr = module.target_config().pointer_type();
    let entry = b.create_block();
    b.append_block_params_for_function_params(entry);
    b.switch_to_block(entry);
    let args = b.block_params(entry).to_vec();
    let cx = args[0];
    let load = |b: &mut FunctionBuilder<'_>, offset| b.ins().load(ptr, FLAGS, cx, offset);
    let (inputs, present) = (
        load(&mut b, offsets::INPUTS),
        load(&mut b, offsets::PRESENT),
    );
    let (outputs, written) = (
        load(&mut b, offsets::OUTPUTS),
        load(&mut b, offsets::WRITTEN),
    );
    let (state, params_ptr) = (load(&mut b, offsets::STATE), load(&mut b, offsets::PARAMS));
    let faults = b.declare_var(types::I32);
    let zero = b.ins().iconst(types::I32, 0);
    b.def_var(faults, zero);
    let mut vars = Vec::new();
    for (index, prim) in locals.iter().enumerate() {
        let var = b.declare_var(clif(*prim));
        let value = match index < params {
            true => args[index + 1],
            false => match prim {
                Prim::F32 => b.ins().f32const(0.0),
                Prim::F64 => b.ins().f64const(0.0),
                p => b.ins().iconst(clif(*p), 0),
            },
        };
        b.def_var(var, value);
        vars.push(var);
    }
    FnCx {
        b,
        module,
        funcs,
        refs: HashMap::new(),
        ptr,
        cx,
        inputs,
        present,
        outputs,
        written,
        state,
        params: params_ptr,
        locals: vars,
        faults,
        seq: None,
        ret,
    }
}

/// Lowers one `def`.
pub(crate) fn define_def<M: Module>(
    module: &mut M,
    funcs: &Funcs,
    id: FuncId,
    def: &hir::Def,
) -> Result<(), CodegenError> {
    let mut ctx = module.make_context();
    ctx.func.signature = entry_sig(module);
    ctx.func.signature.returns.clear();
    ctx.func
        .signature
        .params
        .extend(def.params.iter().map(|p| AbiParam::new(clif(*p))));
    ctx.func
        .signature
        .returns
        .push(AbiParam::new(clif(def.ret)));
    let config = module.target_config();
    let mut fctx = FunctionBuilderContext::new();
    {
        let b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let mut f = open(
            module,
            funcs,
            b,
            &def.body.locals,
            def.params.len(),
            Some(def.ret),
        );
        f.stmts(&def.body.stmts);
        let zero = f.zero(def.ret);
        f.flush_faults();
        f.b.ins().return_(&[zero]);
        f.b.seal_all_blocks();
        f.b.finalize(config);
    }
    module.define_function(id, &mut ctx)?;
    Ok(())
}

/// Lowers one entry point of `ty`.
pub(crate) fn define_entry<M: Module>(
    module: &mut M,
    funcs: &Funcs,
    id: FuncId,
    ty: &hir::Type,
    entry: Entry,
    body: &hir::Body,
) -> Result<(), CodegenError> {
    let mut ctx = module.make_context();
    ctx.func.signature = entry_sig(module);
    let config = module.target_config();
    let mut fctx = FunctionBuilderContext::new();
    {
        let b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
        let mut f = open(module, funcs, b, &body.locals, 0, None);
        if entry != Entry::Plain {
            f.stmts(&ty.prelude);
        }
        let done = match entry {
            Entry::Resume => {
                f.dispatch(ty);
                mc_rt::ResumeWord::Completed as u32
            }
            _ => 0,
        };
        f.stmts(&body.stmts);
        f.ret_const(done);
        f.b.seal_all_blocks();
        f.b.finalize(config);
    }
    module.define_function(id, &mut ctx)?;
    Ok(())
}

impl<M: Module> FnCx<'_, '_, M> {
    /// Jumps to the block the sequence's resume point names.
    fn dispatch(&mut self, ty: &hir::Type) {
        let points: Vec<Block> = (0..=ty.points).map(|_| self.b.create_block()).collect();
        let slot = ty.resume_slot as i32;
        let point = self.b.ins().load(types::I32, FLAGS, self.state, slot);
        let fail = self.b.create_block();
        let calls: Vec<_> = points
            .iter()
            .map(|block| self.b.func.dfg.block_call(*block, &[]))
            .collect();
        let default = self.b.func.dfg.block_call(fail, &[]);
        let table = self
            .b
            .create_jump_table(JumpTableData::new(default, &calls));
        self.b.ins().br_table(point, table);
        self.b.switch_to_block(fail);
        self.ret_const(mc_rt::ResumeWord::Failed as u32);
        self.b.switch_to_block(points[0]);
        self.seq = Some(SeqCx {
            resume_slot: ty.resume_slot,
            fail_slot: ty.desc.fail_slot,
            points,
        });
    }
}
