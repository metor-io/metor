//! Typed IR to wasm, appended to the prelude.
//!
//! Scalar arithmetic is native opcodes; the prelude is entered only where wasm
//! has no instruction — the transcendentals, and `fmod_floor` for float `%`.
//! Two operators cost more than one instruction, and both for the same reason:
//! wasm's `rem` truncates where Python floors, so `//` and `%` on integers
//! carry a correction that adjusts the result when the operands' signs differ.
//!
//! Tensors never touch the wasm stack. Each one has a buffer this module
//! places above the linker's `__heap_base`, so an elementwise call site is
//! `i32.const desc; call $k_add` with every address and shape in `desc` baked
//! into a data segment. That is what "compile-time-constant shapes" and "no
//! guest allocator" both cash out to.
//!
//! Most tensor operations never make that call. Once every address and extent
//! is a constant, so is the kernel's broadcast odometer, and running it here
//! leaves straight-line loads and stores that cost a fifth of the fuel — a
//! length-3 `sum(a + b)` went from 752 to 21. The checker decides per
//! operation, by size ([`Emit`]); the kernels stay for shapes large enough
//! that unrolling would cost more bytes than the fuel is worth.
//!
//! Nothing here re-checks anything. The IR arrived from a single validation
//! gate, so a `Call` index is in range, an operand type is already unified, and
//! a condition is already `bool`.

use std::collections::HashMap;

use wasm_encoder::{BlockType, Function, Instruction, ValType};

use crate::ir::{
    Arith, BufId, Cmp, Desc, Emit, Expr, Func, Intrinsic, Num, Place, Program, Seed, Stmt,
};
use crate::template::Template;
use crate::{COMPILER_VERSION, Init, Manifest, PRELUDE, Ty};

/// Bytes an elementwise descriptor occupies: three addresses, a rank, and
/// three shapes of four axes.
const DESC_BYTES: u32 = 16 * 4;

pub(crate) fn emit(program: &Program, manifest: &Manifest) -> Vec<u8> {
    emit_with(program, manifest, None)
}

/// [`emit`], optionally appending the pack ABI: the prelude's pack helpers
/// join the GC roots and [`pack_abi`](crate::pack_abi) generates the
/// `fsw_pack_*` entry points after the expr exports.
pub(crate) fn emit_with(
    program: &Program,
    manifest: &Manifest,
    pack: Option<&crate::pack::PackPlan>,
) -> Vec<u8> {
    let mut kernels = program.kernels();
    if pack.is_some() {
        kernels.extend_from_slice(crate::pack_abi::PACK_HELPERS);
    }
    let plan = Template::parse(PRELUDE)
        .expect("the checked-in prelude parses")
        .plan(&kernels)
        .expect("the checked-in prelude exports every kernel the IR names");
    let mut splice = plan.splice();

    let kernel_index: HashMap<&'static str, u32> =
        kernels.iter().map(|k| (*k, splice.kernel(k))).collect();
    let base = splice.next_function();

    // Buffers first, then the descriptors kernel call sites point at, all
    // eight-aligned so an f64 load never straddles. A field's parent is always
    // placed before it, so one pass suffices.
    let mut cursor = (splice.heap_base() + 7) & !7;
    let mut addresses: Vec<u32> = Vec::with_capacity(program.buffers.len());
    for place in &program.buffers {
        addresses.push(match place {
            Place::Block(bytes) => {
                let at = cursor;
                cursor += (bytes + 7) & !7;
                at
            }
            Place::Field { parent, offset } => addresses[*parent as usize] + offset,
        });
    }

    let layout = Layout {
        addresses,
        descriptors: cursor,
    };

    let types: Vec<u32> = program
        .funcs
        .iter()
        .map(|func| match (&func.system, func.buffer_abi) {
            (Some(_), _) => splice.ty(vec![ValType::I64], vec![ValType::I32]),
            (None, true) => splice.ty(vec![], vec![ValType::I32]),
            (None, false) => {
                let params: Vec<ValType> = func.locals[..func.param_count]
                    .iter()
                    .map(val_type)
                    .collect();
                splice.ty(params, vec![val_type(&func.ret)])
            }
        })
        .collect();

    let mut descriptors: Vec<u8> = Vec::new();
    let mut indices = Vec::with_capacity(program.funcs.len());
    for (func, ty) in program.funcs.iter().zip(&types) {
        let body = Emitter::new(
            func,
            program,
            &kernel_index,
            base,
            &layout,
            &mut descriptors,
        )
        .finish(func);
        indices.push(splice.function(*ty, body));
    }

    let accessor = splice.ty(vec![ValType::I32], vec![ValType::I32]);
    let nullary = splice.ty(vec![], vec![ValType::I32]);
    for (func, index) in program.funcs.iter().zip(&indices) {
        match &func.system {
            Some(abi) => {
                splice.export(format!("{}_eval", func.name), *index);
                let table = address_table(&layout, &abi.state);
                let state_ptr = splice.function(accessor, table);
                splice.export(format!("{}_state_ptr", func.name), state_ptr);
            }
            None => splice.export(func.name.clone(), *index),
        }
        if !func.buffer_abi {
            continue;
        }
        let table = address_table(&layout, &func.arg_buffers);
        let arg_ptr = splice.function(accessor, table);
        splice.export(format!("{}_arg_ptr", func.name), arg_ptr);

        let mut ret = Function::new([]);
        ret.instruction(&Instruction::I32Const(
            layout.at(func.ret_buffer.expect("a buffer-ABI function has one")) as i32,
        ));
        ret.instruction(&Instruction::End);
        let ret_ptr = splice.function(nullary, ret);
        splice.export(format!("{}_ret_ptr", func.name), ret_ptr);
    }

    if !descriptors.is_empty() {
        splice.data(layout.descriptors, descriptors.clone());
    }

    // Describe, then read: the manifest the host was handed and the one the
    // module carries are the same bytes, so a module that outlives its
    // compilation still says what it is.
    let described = postcard::to_allocvec(manifest).expect("a manifest is postcard-encodable");
    let at = layout.descriptors + descriptors.len() as u32;
    let end = at + described.len() as u32;
    splice.data(at, described.clone());
    for (name, value) in [
        ("expr_abi_version", COMPILER_VERSION as i32),
        ("expr_describe", described.len() as i32),
        ("expr_manifest_ptr", at as i32),
    ] {
        let mut body = Function::new([]);
        body.instruction(&Instruction::I32Const(value));
        body.instruction(&Instruction::End);
        let index = splice.function(nullary, body);
        splice.export(name, index);
    }

    let end = match pack {
        Some(plan) => crate::pack_abi::emit(&mut splice, program, &layout, &indices, plan, end),
        None => end,
    };
    splice.reserve_memory(end);
    splice.finish()
}

/// `<name>_arg_ptr(i)` and `<name>_state_ptr(i)`: a jump table of constants,
/// because every shape is static and so is every address. An index nothing
/// claims answers zero.
fn address_table(layout: &Layout, buffers: &[BufId]) -> Function {
    let mut table = Function::new([]);
    for (i, buf) in buffers.iter().enumerate() {
        table.instruction(&Instruction::LocalGet(0));
        table.instruction(&Instruction::I32Const(i as i32));
        table.instruction(&Instruction::I32Eq);
        table.instruction(&Instruction::If(BlockType::Empty));
        table.instruction(&Instruction::I32Const(layout.at(*buf) as i32));
        table.instruction(&Instruction::Return);
        table.instruction(&Instruction::End);
    }
    table.instruction(&Instruction::I32Const(0));
    table.instruction(&Instruction::End);
    table
}

pub(crate) struct Layout {
    addresses: Vec<u32>,
    descriptors: u32,
}

impl Layout {
    pub(crate) fn at(&self, buf: BufId) -> u32 {
        self.addresses[buf as usize]
    }
}

pub(crate) fn val_type(ty: &Ty) -> ValType {
    match ty {
        Ty::F64 => ValType::F64,
        Ty::I64 => ValType::I64,
        Ty::Bool => ValType::I32,
        Ty::Tensor { .. } => ValType::I32,
    }
}

struct Emitter<'a> {
    locals: Vec<ValType>,
    param_count: usize,
    free: HashMap<ValType, Vec<u32>>,
    code: Vec<Instruction<'static>>,
    depth: u32,
    loops: Vec<(u32, u32)>,
    kernels: &'a HashMap<&'static str, u32>,
    base: u32,
    program: &'a Program,
    layout: &'a Layout,
    descriptors: &'a mut Vec<u8>,
}

impl<'a> Emitter<'a> {
    fn new(
        func: &Func,
        program: &'a Program,
        kernels: &'a HashMap<&'static str, u32>,
        base: u32,
        layout: &'a Layout,
        descriptors: &'a mut Vec<u8>,
    ) -> Self {
        Emitter {
            locals: func.locals.iter().map(val_type).collect(),
            param_count: func.param_count,
            free: HashMap::new(),
            code: Vec::new(),
            depth: 0,
            loops: Vec::new(),
            kernels,
            base,
            program,
            layout,
            descriptors,
        }
    }

    fn finish(mut self, func: &Func) -> Function {
        for (buf, slot, ty) in &func.prologue {
            self.push(Instruction::I32Const(self.layout.at(*buf) as i32));
            self.push(load_of(ty));
            self.push(Instruction::LocalSet(*slot));
        }
        if let Some(abi) = &func.system
            && let Some(guard) = abi.state.last()
        {
            self.seed(self.layout.at(*guard), &abi.seeds);
        }
        self.block(&func.body);
        if func.buffer_abi {
            self.code.push(Instruction::I32Const(0));
            self.code.push(Instruction::Return);
        }
        self.code.push(Instruction::Unreachable);
        self.code.push(Instruction::End);

        let mut runs: Vec<(u32, ValType)> = Vec::new();
        for ty in &self.locals[self.param_count..] {
            match runs.last_mut() {
                Some((count, last)) if last == ty => *count += 1,
                _ => runs.push((1, *ty)),
            }
        }
        let mut body = Function::new(runs);
        for instruction in &self.code {
            body.instruction(instruction);
        }
        body
    }

    fn acquire(&mut self, ty: ValType) -> u32 {
        match self.free.get_mut(&ty).and_then(Vec::pop) {
            Some(slot) => slot,
            None => {
                self.locals.push(ty);
                (self.locals.len() - 1) as u32
            }
        }
    }

    fn release(&mut self, ty: ValType, slot: u32) {
        self.free.entry(ty).or_default().push(slot);
    }

    fn push(&mut self, instruction: Instruction<'static>) {
        self.code.push(instruction);
    }

    fn block(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Assign { local, value } => {
                self.expr(value);
                self.push(Instruction::LocalSet(*local));
            }
            Stmt::Return(value) => {
                self.expr(value);
                self.push(Instruction::Return);
            }
            Stmt::Drop(value) => {
                self.expr(value);
                self.push(Instruction::Drop);
            }
            Stmt::If { cond, then, els } => {
                self.expr(cond);
                self.push(Instruction::If(BlockType::Empty));
                self.depth += 1;
                self.block(then);
                if !els.is_empty() {
                    self.push(Instruction::Else);
                    self.block(els);
                }
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Stmt::While { cond, body } => {
                self.push(Instruction::Block(BlockType::Empty));
                self.depth += 1;
                let break_frame = self.depth;
                self.push(Instruction::Loop(BlockType::Empty));
                self.depth += 1;
                let continue_frame = self.depth;
                self.loops.push((break_frame, continue_frame));

                self.expr(cond);
                self.push(Instruction::I32Eqz);
                let out = self.depth - break_frame;
                self.push(Instruction::BrIf(out));
                self.block(body);
                let back = self.depth - continue_frame;
                self.push(Instruction::Br(back));

                self.loops.pop();
                self.depth -= 1;
                self.push(Instruction::End);
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Stmt::TensorAssign { dest, value, bytes } => {
                self.expr(value);
                if *bytes > 0 {
                    self.copy(self.layout.at(*dest), value.buffer(), *bytes);
                }
            }
            Stmt::ElementAssign {
                target,
                index,
                value,
                len,
            } => {
                self.element_address(target, index, *len);
                self.expr(value);
                self.push(Instruction::F64Store(mem_arg(3)));
            }
            Stmt::ReturnBuffer { dest, value, ty } => {
                self.write(*dest, value, ty);
                self.push(Instruction::I32Const(0));
                self.push(Instruction::Return);
            }
            Stmt::Store { dest, value, ty } => self.write(*dest, value, ty),
            Stmt::ReturnFrame(fields) => {
                for field in fields {
                    self.write(field.dest, &field.value, &field.ty);
                }
                self.push(Instruction::I32Const(0));
                self.push(Instruction::Return);
            }
            Stmt::Break => {
                let (frame, _) = *self.loops.last().expect("the checker rejects a bare break");
                let target = self.depth - frame;
                self.push(Instruction::Br(target));
            }
            Stmt::Continue => {
                let (_, frame) = *self
                    .loops
                    .last()
                    .expect("the checker rejects a bare continue");
                let target = self.depth - frame;
                self.push(Instruction::Br(target));
            }
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::F64(v) => self.push(Instruction::F64Const((*v).into())),
            Expr::I64(v) => self.push(Instruction::I64Const(*v)),
            Expr::Bool(v) => self.push(Instruction::I32Const(*v as i32)),
            Expr::Local(slot) => self.push(Instruction::LocalGet(*slot)),
            Expr::Load { buf, ty } => {
                self.push(Instruction::I32Const(self.layout.at(*buf) as i32));
                self.push(load_of(ty));
            }
            Expr::Address(buf) => self.push(Instruction::I32Const(self.layout.at(*buf) as i32)),
            Expr::Store { local, value, then } => {
                self.expr(value);
                self.push(Instruction::LocalSet(*local));
                self.expr(then);
            }
            Expr::Arith { op, ty, lhs, rhs } => self.arith(*op, *ty, lhs, rhs),
            Expr::PowConst { ty, base, exp } => self.pow_const(*ty, base, *exp),
            Expr::Cmp { op, ty, lhs, rhs } => {
                self.expr(lhs);
                self.expr(rhs);
                self.push(compare(*op, *ty));
            }
            Expr::CmpBool { eq, lhs, rhs } => {
                self.expr(lhs);
                self.expr(rhs);
                self.push(if *eq {
                    Instruction::I32Eq
                } else {
                    Instruction::I32Ne
                });
            }
            Expr::Not(inner) => {
                self.expr(inner);
                self.push(Instruction::I32Eqz);
            }
            Expr::And(lhs, rhs) => {
                self.expr(lhs);
                self.push(Instruction::If(BlockType::Result(ValType::I32)));
                self.depth += 1;
                self.expr(rhs);
                self.push(Instruction::Else);
                self.push(Instruction::I32Const(0));
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Expr::Or(lhs, rhs) => {
                self.expr(lhs);
                self.push(Instruction::If(BlockType::Result(ValType::I32)));
                self.depth += 1;
                self.push(Instruction::I32Const(1));
                self.push(Instruction::Else);
                self.expr(rhs);
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Expr::Select {
                cond,
                then,
                els,
                ty,
            } => {
                self.expr(cond);
                self.push(Instruction::If(BlockType::Result(val_type(ty))));
                self.depth += 1;
                self.expr(then);
                self.push(Instruction::Else);
                self.expr(els);
                self.depth -= 1;
                self.push(Instruction::End);
            }
            Expr::Call { index, args, .. } => {
                for arg in args {
                    self.expr(arg);
                }
                self.push(Instruction::Call(self.base + index));
            }
            Expr::Kernel { name, args, .. } => {
                for arg in args {
                    self.expr(arg);
                }
                self.push(Instruction::Call(self.kernels[name]));
            }
            Expr::Intrinsic { op, args } => self.intrinsic(*op, args),

            // A tensor is already where it belongs; naming one costs nothing.
            Expr::Tensor(_) => {}
            Expr::Elementwise {
                kernel,
                dest,
                lhs,
                rhs,
                desc,
                emit,
            } => self.elementwise(kernel, *dest, lhs, rhs, desc, *emit),
            Expr::Splat { dest, value } => {
                self.push(Instruction::I32Const(self.layout.at(*dest) as i32));
                self.expr(value);
                self.push(Instruction::F64Store(mem_arg(3)));
            }
            // A literal's shape is static and a tensor is contiguous, so this
            // is one store per element at a constant address — the same shape
            // as a splat, repeated.
            Expr::TensorLit { dest, elements } => self.tensor_lit(*dest, elements),
            Expr::TensorNeg {
                dest,
                operand,
                elems,
                emit,
            } => {
                self.expr(operand);
                let source = self.layout.at(operand.buffer());
                let out = self.layout.at(*dest);
                match emit {
                    Emit::Kernel => {
                        self.push(Instruction::I32Const(source as i32));
                        self.push(Instruction::I32Const(out as i32));
                        self.push(Instruction::I32Const(*elems as i32));
                        self.push(Instruction::Call(self.kernels["k_neg"]));
                    }
                    Emit::Open => {
                        for i in 0..*elems {
                            self.push(Instruction::I32Const((out + i * 8) as i32));
                            self.load(source + i * 8);
                            self.push(Instruction::F64Neg);
                            self.push(Instruction::F64Store(mem_arg(3)));
                        }
                    }
                }
            }
            Expr::CheckedIndex { value, len } => {
                let checked = self.acquire(ValType::I64);
                self.expr(value);
                self.push(Instruction::LocalTee(checked));
                self.push(Instruction::I64Const(*len as i64));
                self.push(Instruction::I64GeU);
                self.push(Instruction::If(BlockType::Empty));
                self.push(Instruction::Unreachable);
                self.push(Instruction::End);
                self.push(Instruction::LocalGet(checked));
                self.release(ValType::I64, checked);
            }
            Expr::Element { source, index, len } => {
                self.element_address(source, index, *len);
                self.push(Instruction::F64Load(mem_arg(3)));
            }
            Expr::Dot {
                lhs,
                rhs,
                len,
                emit,
            } => {
                self.expr(lhs);
                self.expr(rhs);
                let a = self.layout.at(lhs.buffer());
                let b = self.layout.at(rhs.buffer());
                match emit {
                    Emit::Kernel => {
                        self.push(Instruction::I32Const(a as i32));
                        self.push(Instruction::I32Const(b as i32));
                        self.push(Instruction::I32Const(*len as i32));
                        self.push(Instruction::Call(self.kernels["k_dot"]));
                    }
                    Emit::Open => {
                        self.push(Instruction::F64Const(0.0.into()));
                        for i in 0..*len {
                            self.load(a + i * 8);
                            self.load(b + i * 8);
                            self.push(Instruction::F64Mul);
                            self.push(Instruction::F64Add);
                        }
                    }
                }
            }
            Expr::MatMul {
                dest,
                lhs,
                rhs,
                m,
                k,
                n,
                batches,
                emit,
            } => {
                self.expr(lhs);
                self.expr(rhs);
                let a = self.layout.at(lhs.buffer());
                let b = self.layout.at(rhs.buffer());
                let out = self.layout.at(*dest);
                // Leading dimensions were walked while checking, so a batch is
                // three more constants rather than a loop.
                for batch in batches {
                    let (a, b, out) = (a + batch.lhs * 8, b + batch.rhs * 8, out + batch.out * 8);
                    match emit {
                        Emit::Kernel => {
                            self.push(Instruction::I32Const(a as i32));
                            self.push(Instruction::I32Const(b as i32));
                            self.push(Instruction::I32Const(out as i32));
                            for extent in [m, k, n] {
                                self.push(Instruction::I32Const(*extent as i32));
                            }
                            self.push(Instruction::Call(self.kernels["k_matmul"]));
                        }
                        Emit::Open => {
                            for row in 0..*m {
                                for col in 0..*n {
                                    self.push(Instruction::I32Const(
                                        (out + (row * n + col) * 8) as i32,
                                    ));
                                    self.push(Instruction::F64Const(0.0.into()));
                                    for i in 0..*k {
                                        self.load(a + (row * k + i) * 8);
                                        self.load(b + (i * n + col) * 8);
                                        self.push(Instruction::F64Mul);
                                        self.push(Instruction::F64Add);
                                    }
                                    self.push(Instruction::F64Store(mem_arg(3)));
                                }
                            }
                        }
                    }
                }
            }
            Expr::Sum {
                source,
                elems,
                emit,
            } => {
                self.expr(source);
                let at = self.layout.at(source.buffer());
                match emit {
                    Emit::Kernel => {
                        self.push(Instruction::I32Const(at as i32));
                        self.push(Instruction::I32Const(*elems as i32));
                        self.push(Instruction::Call(self.kernels["k_sum"]));
                    }
                    Emit::Open => {
                        self.push(Instruction::F64Const(0.0.into()));
                        for i in 0..*elems {
                            self.load(at + i * 8);
                            self.push(Instruction::F64Add);
                        }
                    }
                }
            }
            Expr::Window {
                state,
                value,
                elems,
                len,
            } => {
                // Shift the ring down by one sample and write the new one at
                // the end, so the newest is last. Both addresses are
                // constants, so the shift is one `memory.copy`.
                let at = self.layout.at(*state);
                let stride = elems * 8;
                self.push(Instruction::I32Const(at as i32));
                self.push(Instruction::I32Const((at + stride) as i32));
                self.push(Instruction::I32Const(((len - 1) * stride) as i32));
                self.push(Instruction::MemoryCopy {
                    src_mem: 0,
                    dst_mem: 0,
                });
                let last = at + (len - 1) * stride;
                self.expr(value);
                match *elems {
                    1 => {
                        let slot = self.acquire(ValType::F64);
                        self.push(Instruction::LocalSet(slot));
                        self.push(Instruction::I32Const(last as i32));
                        self.push(Instruction::LocalGet(slot));
                        self.push(Instruction::F64Store(mem_arg(3)));
                        self.release(ValType::F64, slot);
                    }
                    _ => self.copy(last, value.buffer(), stride),
                }
            }
            Expr::Fft {
                dest,
                source,
                scratch,
                len,
                groups,
            } => {
                self.expr(source);
                for at in [
                    self.layout.at(source.buffer()),
                    self.layout.at(*dest),
                    self.layout.at(*scratch),
                ] {
                    self.push(Instruction::I32Const(at as i32));
                }
                self.push(Instruction::I32Const(*len as i32));
                self.push(Instruction::I32Const(*groups as i32));
                self.push(Instruction::Call(self.kernels["k_fft"]));
            }
            Expr::BufferCall {
                index,
                args,
                dest,
                ret,
            } => {
                let callee = &self.program.funcs[*index as usize];
                let params = &self.program.param_types[*index as usize];
                for ((arg, buf), ty) in args.iter().zip(&callee.arg_buffers).zip(params) {
                    self.expr(arg);
                    match ty {
                        Ty::Tensor { .. } => {
                            self.copy(self.layout.at(*buf), arg.buffer(), slot_bytes(ty))
                        }
                        _ => {
                            let slot = self.acquire(val_type(ty));
                            self.push(Instruction::LocalSet(slot));
                            self.push(Instruction::I32Const(self.layout.at(*buf) as i32));
                            self.push(Instruction::LocalGet(slot));
                            self.push(store_of(ty));
                            self.release(val_type(ty), slot);
                        }
                    }
                }
                self.push(Instruction::Call(self.base + index));
                self.push(Instruction::Drop);
                let ret_buffer = callee.ret_buffer.expect("a buffer-ABI function has one");
                match (ret, dest) {
                    // The callee's result buffer is reused by its next call,
                    // so the caller takes a copy it owns.
                    (Ty::Tensor { .. }, Some(dest)) => {
                        self.copy(self.layout.at(*dest), ret_buffer, slot_bytes(ret))
                    }
                    _ => {
                        self.push(Instruction::I32Const(self.layout.at(ret_buffer) as i32));
                        self.push(load_of(ret));
                    }
                }
            }
        }
    }

    /// The first evaluation's one extra job: write the annotated defaults into
    /// the state slots, once.
    ///
    /// The guard is why instantiation needs no host-side init walk, and it is
    /// also what makes restore safe — a host that seeds a rebuilt instance
    /// from a snapshot writes the guard along with the slots, and this never
    /// runs.
    fn seed(&mut self, guard: u32, seeds: &[Seed]) {
        self.push(Instruction::I32Const(guard as i32));
        self.push(Instruction::I32Load(mem_arg(2)));
        self.push(Instruction::I32Eqz);
        self.push(Instruction::If(BlockType::Empty));
        self.depth += 1;
        self.push(Instruction::I32Const(guard as i32));
        self.push(Instruction::I32Const(1));
        self.push(Instruction::I32Store(mem_arg(2)));
        for seed in seeds {
            let at = self.layout.at(seed.dest);
            let (constant, store, stride) = match seed.value {
                Init::F64(v) | Init::Fill(v) => (
                    Instruction::F64Const(v.into()),
                    Instruction::F64Store(mem_arg(3)),
                    8,
                ),
                Init::I64(v) => (
                    Instruction::I64Const(v),
                    Instruction::I64Store(mem_arg(3)),
                    8,
                ),
                Init::Bool(v) => (
                    Instruction::I32Const(v as i32),
                    Instruction::I32Store(mem_arg(2)),
                    8,
                ),
            };
            for i in 0..slot_bytes(&seed.ty) / stride {
                self.push(Instruction::I32Const((at + i * stride) as i32));
                self.push(constant.clone());
                self.push(store.clone());
            }
        }
        self.depth -= 1;
        self.push(Instruction::End);
    }

    /// Evaluate a value and put it in a buffer — a tensor by copy, a scalar
    /// through a slot, since a store wants its address underneath its value.
    fn write(&mut self, dest: BufId, value: &Expr, ty: &Ty) {
        self.expr(value);
        match ty {
            Ty::Tensor { .. } => self.copy(self.layout.at(dest), value.buffer(), slot_bytes(ty)),
            _ => {
                let slot = self.acquire(val_type(ty));
                self.push(Instruction::LocalSet(slot));
                self.push(Instruction::I32Const(self.layout.at(dest) as i32));
                self.push(Instruction::LocalGet(slot));
                self.push(store_of(ty));
                self.release(val_type(ty), slot);
            }
        }
    }

    /// A literal's shape is static and a tensor is contiguous, so this is one
    /// store per element at a constant address — a splat, repeated.
    fn tensor_lit(&mut self, dest: BufId, elements: &[Expr]) {
        let at = self.layout.at(dest);
        for (i, element) in elements.iter().enumerate() {
            self.push(Instruction::I32Const((at + i as u32 * 8) as i32));
            self.expr(element);
            self.push(Instruction::F64Store(mem_arg(3)));
        }
    }

    /// Elementwise arithmetic, by whichever path the checker chose.
    ///
    /// This is a method rather than a match arm for a reason that only shows
    /// up in a debug build: every arm's locals are live for the whole of
    /// [`Emitter::expr`], and a deeply nested expression pays for all of them
    /// at every level. The arms that carry the most — this one and the
    /// literal above — are what a two-hundred-term chain could not afford.
    fn elementwise(
        &mut self,
        kernel: &'static str,
        dest: BufId,
        lhs: &Expr,
        rhs: &Expr,
        desc: &Desc,
        emit: Emit,
    ) {
        self.expr(lhs);
        self.expr(rhs);
        match emit {
            Emit::Kernel => {
                let at = self.descriptor(lhs.buffer(), rhs.buffer(), dest, desc);
                self.push(Instruction::I32Const(at as i32));
                self.push(Instruction::Call(self.kernels[kernel]));
            }
            Emit::Open => {
                let a = self.layout.at(lhs.buffer());
                let b = self.layout.at(rhs.buffer());
                let out = self.layout.at(dest);
                for (i, (l, r)) in broadcast_pairs(desc).into_iter().enumerate() {
                    self.push(Instruction::I32Const((out + i as u32 * 8) as i32));
                    self.load(a + l * 8);
                    self.load(b + r * 8);
                    self.scalar_of(kernel);
                    self.push(Instruction::F64Store(mem_arg(3)));
                }
            }
        }
    }

    /// Push the `f64` at a constant address.
    fn load(&mut self, at: u32) {
        self.push(Instruction::I32Const(at as i32));
        self.push(Instruction::F64Load(mem_arg(3)));
    }

    /// The scalar form of an elementwise kernel. `pow` and `atan2` have no
    /// instruction, so open coding calls them once per element — which is what
    /// the kernel was doing anyway, minus the odometer around it.
    fn scalar_of(&mut self, kernel: &str) {
        match kernel {
            "k_add" => self.push(Instruction::F64Add),
            "k_sub" => self.push(Instruction::F64Sub),
            "k_mul" => self.push(Instruction::F64Mul),
            "k_div" => self.push(Instruction::F64Div),
            "k_pow" => self.push(Instruction::Call(self.kernels["pow"])),
            "k_atan2" => self.push(Instruction::Call(self.kernels["atan2"])),
            other => unreachable!("`{other}` is not an elementwise kernel"),
        }
    }

    /// `dest` and `source` are both constants, so a copy is three constants
    /// and one instruction.
    fn copy(&mut self, dest: u32, source: BufId, bytes: u32) {
        self.push(Instruction::I32Const(dest as i32));
        self.push(Instruction::I32Const(self.layout.at(source) as i32));
        self.push(Instruction::I32Const(bytes as i32));
        self.push(Instruction::MemoryCopy {
            src_mem: 0,
            dst_mem: 0,
        });
    }

    /// Leaves the address of element `index` of `source` on the stack, after
    /// trapping if the index is out of range. An unsigned compare catches
    /// negatives too, which is why `v[-1]` traps rather than wrapping around.
    fn element_address(&mut self, source: &Expr, index: &Expr, len: u32) {
        self.expr(source);
        let base = self.layout.at(source.buffer());
        if let Expr::I64(constant) = index {
            self.push(Instruction::I32Const(base as i32 + *constant as i32 * 8));
            return;
        }
        let slot = self.acquire(ValType::I64);
        self.expr(index);
        self.push(Instruction::LocalTee(slot));
        self.push(Instruction::I64Const(len as i64));
        self.push(Instruction::I64GeU);
        self.push(Instruction::If(BlockType::Empty));
        self.depth += 1;
        self.push(Instruction::Unreachable);
        self.depth -= 1;
        self.push(Instruction::End);
        self.push(Instruction::LocalGet(slot));
        self.push(Instruction::I32WrapI64);
        self.push(Instruction::I32Const(8));
        self.push(Instruction::I32Mul);
        self.push(Instruction::I32Const(base as i32));
        self.push(Instruction::I32Add);
        self.release(ValType::I64, slot);
    }

    /// Intern a kernel descriptor, so two identical call sites share one.
    fn descriptor(&mut self, lhs: BufId, rhs: BufId, dest: BufId, desc: &Desc) -> u32 {
        let mut bytes = Vec::with_capacity(DESC_BYTES as usize);
        for word in [
            self.layout.at(lhs),
            self.layout.at(rhs),
            self.layout.at(dest),
            desc.rank,
        ] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        for shape in [&desc.lhs, &desc.rhs, &desc.out] {
            for axis in 0..4 {
                let extent = shape.get(axis).copied().unwrap_or(0);
                bytes.extend_from_slice(&extent.to_le_bytes());
            }
        }
        if let Some(at) = self
            .descriptors
            .chunks_exact(DESC_BYTES as usize)
            .position(|existing| existing == bytes)
        {
            return self.layout.descriptors + at as u32 * DESC_BYTES;
        }
        let at = self.layout.descriptors + self.descriptors.len() as u32;
        self.descriptors.extend_from_slice(&bytes);
        at
    }

    fn arith(&mut self, op: Arith, ty: Num, lhs: &Expr, rhs: &Expr) {
        match (op, ty) {
            (Arith::Add, Num::F64) => self.simple(lhs, rhs, Instruction::F64Add),
            (Arith::Add, Num::I64) => self.simple(lhs, rhs, Instruction::I64Add),
            (Arith::Sub, Num::F64) => self.simple(lhs, rhs, Instruction::F64Sub),
            (Arith::Sub, Num::I64) => self.simple(lhs, rhs, Instruction::I64Sub),
            (Arith::Mul, Num::F64) => self.simple(lhs, rhs, Instruction::F64Mul),
            (Arith::Mul, Num::I64) => self.simple(lhs, rhs, Instruction::I64Mul),
            (Arith::Div, _) => self.simple(lhs, rhs, Instruction::F64Div),
            (Arith::FloorDiv, Num::F64) => {
                self.simple(lhs, rhs, Instruction::F64Div);
                self.push(Instruction::F64Floor);
            }
            (Arith::FloorDiv, Num::I64) => self.floor_div_i64(lhs, rhs),
            (Arith::Rem, Num::I64) => self.floor_rem_i64(lhs, rhs),
            (Arith::Rem, Num::F64) => unreachable!("float `%` lowers to the fmod_floor kernel"),
        }
    }

    fn simple(&mut self, lhs: &Expr, rhs: &Expr, op: Instruction<'static>) {
        self.expr(lhs);
        self.expr(rhs);
        self.push(op);
    }

    /// Emit wrapping floor division with Python's sign rule.
    fn floor_div_i64(&mut self, lhs: &Expr, rhs: &Expr) {
        let (a, b, r) = (
            self.acquire(ValType::I64),
            self.acquire(ValType::I64),
            self.acquire(ValType::I64),
        );
        self.expr(lhs);
        self.push(Instruction::LocalSet(a));
        self.expr(rhs);
        self.push(Instruction::LocalSet(b));

        self.push(Instruction::LocalGet(a));
        self.push(Instruction::I64Const(i64::MIN));
        self.push(Instruction::I64Eq);
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64Const(-1));
        self.push(Instruction::I64Eq);
        self.push(Instruction::I32And);
        self.push(Instruction::If(BlockType::Result(ValType::I64)));
        self.push(Instruction::I64Const(i64::MIN));
        self.push(Instruction::Else);
        self.push(Instruction::LocalGet(a));
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64DivS);

        self.push(Instruction::LocalGet(a));
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64RemS);
        self.push(Instruction::LocalSet(r));
        self.signs_differ(b, r);
        self.push(Instruction::I64ExtendI32U);
        self.push(Instruction::I64Sub);
        self.push(Instruction::End);

        self.release(ValType::I64, a);
        self.release(ValType::I64, b);
        self.release(ValType::I64, r);
    }

    /// `a % b` with Python's sign rule: add the divisor back when the
    /// truncating remainder landed on the wrong side of zero.
    fn floor_rem_i64(&mut self, lhs: &Expr, rhs: &Expr) {
        let (a, b, r) = (
            self.acquire(ValType::I64),
            self.acquire(ValType::I64),
            self.acquire(ValType::I64),
        );
        self.expr(lhs);
        self.push(Instruction::LocalSet(a));
        self.expr(rhs);
        self.push(Instruction::LocalSet(b));

        self.push(Instruction::LocalGet(a));
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64RemS);
        self.push(Instruction::LocalTee(r));

        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64Const(0));
        self.signs_differ(b, r);
        self.push(Instruction::Select);
        self.push(Instruction::I64Add);

        self.release(ValType::I64, a);
        self.release(ValType::I64, b);
        self.release(ValType::I64, r);
    }

    /// Leaves `1` on the stack when the remainder in `r` is non-zero and its
    /// sign disagrees with the divisor in `b`.
    fn signs_differ(&mut self, b: u32, r: u32) {
        self.push(Instruction::LocalGet(r));
        self.push(Instruction::I64Eqz);
        self.push(Instruction::I32Eqz);
        self.push(Instruction::LocalGet(r));
        self.push(Instruction::LocalGet(b));
        self.push(Instruction::I64Xor);
        self.push(Instruction::I64Const(0));
        self.push(Instruction::I64LtS);
        self.push(Instruction::I32And);
    }

    fn pow_const(&mut self, ty: Num, base: &Expr, exp: u32) {
        let one = match ty {
            Num::F64 => Instruction::F64Const(1.0.into()),
            Num::I64 => Instruction::I64Const(1),
        };
        if exp == 0 {
            self.push(one);
            return;
        }
        if exp == 1 {
            self.expr(base);
            return;
        }
        let slot = self.acquire(val_type_of(ty));
        self.expr(base);
        self.push(Instruction::LocalSet(slot));
        self.push(Instruction::LocalGet(slot));
        for _ in 1..exp {
            self.push(Instruction::LocalGet(slot));
            self.push(match ty {
                Num::F64 => Instruction::F64Mul,
                Num::I64 => Instruction::I64Mul,
            });
        }
        self.release(val_type_of(ty), slot);
    }

    fn intrinsic(&mut self, op: Intrinsic, args: &[Expr]) {
        match op {
            Intrinsic::NegF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Neg);
            }
            Intrinsic::NegI64 => {
                self.push(Instruction::I64Const(0));
                self.expr(&args[0]);
                self.push(Instruction::I64Sub);
            }
            Intrinsic::AbsF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Abs);
            }
            Intrinsic::AbsI64 => {
                let slot = self.acquire(ValType::I64);
                self.expr(&args[0]);
                self.push(Instruction::LocalSet(slot));
                self.push(Instruction::LocalGet(slot));
                self.push(Instruction::I64Const(0));
                self.push(Instruction::LocalGet(slot));
                self.push(Instruction::I64Sub);
                self.push(Instruction::LocalGet(slot));
                self.push(Instruction::I64Const(0));
                self.push(Instruction::I64GeS);
                self.push(Instruction::Select);
                self.release(ValType::I64, slot);
            }
            Intrinsic::SqrtF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Sqrt);
            }
            Intrinsic::FloorF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Floor);
            }
            Intrinsic::CeilF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Ceil);
            }
            Intrinsic::TruncF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Trunc);
            }
            Intrinsic::NearestF64 => {
                self.expr(&args[0]);
                self.push(Instruction::F64Nearest);
            }
            Intrinsic::MinF64 => self.simple(&args[0], &args[1], Instruction::F64Min),
            Intrinsic::MaxF64 => self.simple(&args[0], &args[1], Instruction::F64Max),
            Intrinsic::MinI64 | Intrinsic::MaxI64 => {
                let (a, b) = (self.acquire(ValType::I64), self.acquire(ValType::I64));
                self.expr(&args[0]);
                self.push(Instruction::LocalSet(a));
                self.expr(&args[1]);
                self.push(Instruction::LocalSet(b));
                self.push(Instruction::LocalGet(a));
                self.push(Instruction::LocalGet(b));
                self.push(Instruction::LocalGet(a));
                self.push(Instruction::LocalGet(b));
                self.push(if op == Intrinsic::MinI64 {
                    Instruction::I64LtS
                } else {
                    Instruction::I64GtS
                });
                self.push(Instruction::Select);
                self.release(ValType::I64, a);
                self.release(ValType::I64, b);
            }
            Intrinsic::IntToFloat => {
                self.expr(&args[0]);
                self.push(Instruction::F64ConvertI64S);
            }
            Intrinsic::FloatToInt => {
                self.expr(&args[0]);
                self.push(Instruction::I64TruncF64S);
            }
        }
    }
}

/// Which element of each operand feeds each output element — `k_add`'s
/// odometer, run here instead of at every call.
///
/// The shapes in a [`Desc`] are already right-aligned to a common rank, so an
/// axis of extent one simply contributes stride zero, exactly as the kernel's
/// `broadcast_strides` arranges.
fn broadcast_pairs(desc: &Desc) -> Vec<(u32, u32)> {
    let rank = desc.rank as usize;
    let strides = |shape: &[u32]| {
        let mut row = vec![0u32; rank];
        let mut acc = 1;
        for axis in (0..rank).rev() {
            row[axis] = if shape[axis] == 1 { 0 } else { acc };
            acc *= shape[axis];
        }
        row
    };
    let (lhs, rhs) = (strides(&desc.lhs), strides(&desc.rhs));

    let mut index = vec![0u32; rank];
    let mut pairs = Vec::with_capacity(desc.out.iter().product::<u32>() as usize);
    for _ in 0..desc.out.iter().product::<u32>() {
        pairs.push((0..rank).fold((0, 0), |(l, r), axis| {
            (l + index[axis] * lhs[axis], r + index[axis] * rhs[axis])
        }));
        for axis in (0..rank).rev() {
            index[axis] += 1;
            if index[axis] < desc.out[axis] {
                break;
            }
            index[axis] = 0;
        }
    }
    pairs
}

fn val_type_of(ty: Num) -> ValType {
    match ty {
        Num::F64 => ValType::F64,
        Num::I64 => ValType::I64,
    }
}

fn compare(op: Cmp, ty: Num) -> Instruction<'static> {
    match (op, ty) {
        (Cmp::Eq, Num::F64) => Instruction::F64Eq,
        (Cmp::Ne, Num::F64) => Instruction::F64Ne,
        (Cmp::Lt, Num::F64) => Instruction::F64Lt,
        (Cmp::Le, Num::F64) => Instruction::F64Le,
        (Cmp::Gt, Num::F64) => Instruction::F64Gt,
        (Cmp::Ge, Num::F64) => Instruction::F64Ge,
        (Cmp::Eq, Num::I64) => Instruction::I64Eq,
        (Cmp::Ne, Num::I64) => Instruction::I64Ne,
        (Cmp::Lt, Num::I64) => Instruction::I64LtS,
        (Cmp::Le, Num::I64) => Instruction::I64LeS,
        (Cmp::Gt, Num::I64) => Instruction::I64GtS,
        (Cmp::Ge, Num::I64) => Instruction::I64GeS,
    }
}

fn mem_arg(align: u32) -> wasm_encoder::MemArg {
    wasm_encoder::MemArg {
        offset: 0,
        align,
        memory_index: 0,
    }
}

fn load_of(ty: &Ty) -> Instruction<'static> {
    match ty {
        Ty::F64 => Instruction::F64Load(mem_arg(3)),
        Ty::I64 => Instruction::I64Load(mem_arg(3)),
        Ty::Bool => Instruction::I32Load(mem_arg(2)),
        Ty::Tensor { .. } => unreachable!("tensors are addressed, not loaded"),
    }
}

fn store_of(ty: &Ty) -> Instruction<'static> {
    match ty {
        Ty::F64 => Instruction::F64Store(mem_arg(3)),
        Ty::I64 => Instruction::I64Store(mem_arg(3)),
        Ty::Bool => Instruction::I32Store(mem_arg(2)),
        Ty::Tensor { .. } => unreachable!("tensors are copied, not stored"),
    }
}

fn slot_bytes(ty: &Ty) -> u32 {
    match ty {
        Ty::Tensor { shape, .. } => (shape.iter().product::<usize>() as u32) * 8,
        _ => 8,
    }
}
